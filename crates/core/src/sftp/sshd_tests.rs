//! 显式启用本机 sshd 后复用原有文件测试；夹具和大文件随测试根目录自动删除。
use super::*;
use crate::builtin_test_support::{Fixture, sshd};
use std::{cell::RefCell, sync::Arc, time::Instant};

thread_local! {
    static SERVERS: RefCell<Vec<Fixture>> = const { RefCell::new(Vec::new()) };
}

// 设置 PIERLINK_TEST_BUILTIN_SFTP=1 后，原有真实文件与协议故障用例改走内置 SSH。
// 每个测试线程持有夹具，结束时先关连接、再停止 sshd 并删除临时文件。
pub(super) fn open(
    program: &SshProgram,
    command: &SshCommand,
    timeout: Duration,
) -> Result<SftpSession, AppError> {
    if std::env::var_os("PIERLINK_TEST_BUILTIN_SFTP").as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return SftpSession::open(program, command, timeout);
    }
    let root = crate::test_support::tempdir();
    let quote = crate::ssh::quote::sh_quote;
    let mut argv = vec![quote(program.path.to_str().unwrap()).unwrap()];
    argv.extend(command.args.iter().map(|arg| quote(arg).unwrap()));
    let launcher = root.path().join("subsystem.sh");
    std::fs::write(
        &launcher,
        format!(
            "cd {} || exit 1\nexec {}\n",
            quote(std::env::current_dir().unwrap().to_str().unwrap()).unwrap(),
            argv.join(" ")
        ),
    )
    .unwrap();
    let f = Fixture::from_sshd(sshd::Server::with_root(
        root,
        &format!(
            "ForceCommand /bin/sh {}",
            quote(launcher.to_str().unwrap()).unwrap()
        ),
        false,
    ));
    let result = SftpSession::open(&f.program(), &f.sftp_command(), timeout);
    SERVERS.with(|servers| servers.borrow_mut().push(f));
    result
}

#[test]
#[ignore = "需要普通用户启动本机 sshd 和随机监听端口"]
fn 内置真实并发传输取消与批量断线后重开() {
    let f = Fixture::from_sshd(sshd::Server::with_root(
        crate::test_support::tempdir(),
        "",
        false,
    ));
    let cmd = f.sftp_command();
    let directory = f.sshd.root.path().to_str().unwrap();
    std::thread::scope(|scope| {
        for index in 0..4 {
            let cmd = &cmd;
            let f = &f;
            scope.spawn(move || {
                let mut session =
                    SftpSession::open(&f.program(), cmd, Duration::from_secs(5)).unwrap();
                let path = format!("{directory}/file{index}");
                let bytes = vec![index as u8; 2 * 1024 * 1024 + 11];
                let cancel = AtomicBool::new(false);
                session
                    .write_from(&path, &mut &bytes[..], None, &mut |_| {}, &cancel)
                    .unwrap();
                let mut actual = vec![];
                session
                    .read_to(&path, &mut actual, &mut |_| {}, &cancel)
                    .unwrap();
                assert_eq!(actual, bytes);
                let cancel = AtomicBool::new(false);
                assert_eq!(
                    session
                        .read_to(
                            &path,
                            &mut std::io::sink(),
                            &mut |_| cancel.store(true, Ordering::Release),
                            &cancel
                        )
                        .unwrap_err()
                        .code,
                    "sftp.cancelled"
                );
                assert!(session.stat(&path).unwrap().is_some());
            });
        }
    });
    let mut session = SftpSession::open(&f.program(), &cmd, Duration::from_secs(5)).unwrap();
    let context = cmd.builtin.as_ref().unwrap();
    let mut request = context.request();
    request.class = crate::builtin::ConnectionClass::Bulk;
    let bulk = f.manager.get_connection(request).unwrap();
    bulk.disconnect().unwrap();
    assert_eq!(session.stat(".").unwrap_err().code, "sftp.connection_lost");
    let mut session = SftpSession::open(&f.program(), &cmd, Duration::from_secs(5)).unwrap();
    assert!(session.stat(directory).unwrap().is_some());
}

#[test]
#[ignore = "需要本机 sshd、约 1 GiB 临时空间及大文件传输时间"]
fn 内置一吉字节上传下载哈希一致且终端保持响应() {
    use sha2::{Digest, Sha256};
    struct Source {
        remaining: u64,
        hash: Sha256,
    }
    impl Read for Source {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let count = (self.remaining.min(buffer.len() as u64)) as usize;
            for (offset, byte) in buffer[..count].iter_mut().enumerate() {
                *byte = ((self.remaining - offset as u64) % 251) as u8;
            }
            self.hash.update(&buffer[..count]);
            self.remaining -= count as u64;
            Ok(count)
        }
    }
    struct Target {
        size: u64,
        hash: Sha256,
    }
    impl Write for Target {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.size += bytes.len() as u64;
            self.hash.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let f = Fixture::from_sshd(sshd::Server::with_root(
        crate::test_support::tempdir(),
        "",
        false,
    ));
    let cmd = f.sftp_command();
    let context = cmd.builtin.as_ref().unwrap();
    let main = f.manager.get_connection(context.request()).unwrap();
    let mut terminal = main
        .terminal(
            &pierlink_ssh_engine::Pty::default(),
            &[],
            Some("stty -echo; while IFS= read -r line; do printf 'response:%s\\n' \"$line\"; done"),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let (mut input, output, stderr) = terminal.take_io();
    drop(stderr);
    let done = Arc::new(AtomicBool::new(false));
    struct Stop(Arc<AtomicBool>);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let cancel = AtomicBool::new(false);
    let path = f.sshd.root.path().join("large");
    std::thread::scope(|scope| {
        let done_reader = done.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let closer = terminal.closer();
        let worker = scope.spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                if line.trim() == "response:probe" {
                    let _ = tx.send(());
                }
                if done_reader.load(Ordering::Acquire) {
                    break;
                }
            }
        });
        let ping_done = done.clone();
        let ping = scope.spawn(move || {
            let mut count = 0;
            while !ping_done.load(Ordering::Acquire) {
                input.write_all(b"probe\n").unwrap();
                rx.recv_timeout(Duration::from_secs(3))
                    .expect("传输期间终端没有及时响应");
                count += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            count
        });
        let start = Instant::now();
        // panic 时同样通知并关闭终端，避免 scope 等待输出线程。
        struct Close(pierlink_ssh_engine::ChannelCloser);
        impl Drop for Close {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let close = Close(closer);
        let stop = Stop(done.clone());
        let mut session = SftpSession::open(&f.program(), &cmd, Duration::from_secs(10)).unwrap();
        let mut source = Source {
            remaining: 1024 * 1024 * 1024,
            hash: Sha256::new(),
        };
        assert_eq!(
            session
                .write_from(
                    path.to_str().unwrap(),
                    &mut source,
                    None,
                    &mut |_| {},
                    &cancel
                )
                .unwrap(),
            1024 * 1024 * 1024
        );
        let mut target = Target {
            size: 0,
            hash: Sha256::new(),
        };
        assert_eq!(
            session
                .read_to(path.to_str().unwrap(), &mut target, &mut |_| {}, &cancel)
                .unwrap(),
            1024 * 1024 * 1024
        );
        assert_eq!(source.hash.finalize(), target.hash.finalize());
        assert_eq!(target.size, 1024 * 1024 * 1024);
        assert_eq!(main.statistics().opened_sessions, 1);
        drop(stop);
        assert!(ping.join().unwrap() > 0);
        drop(close);
        worker.join().unwrap();
        eprintln!(
            "一吉字节双向传输及终端响应验证完成，用时 {:?}",
            start.elapsed()
        );
    });
}
