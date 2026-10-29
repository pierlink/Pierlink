//! 本机 sshd 终端测试；复用连接测试的认证和临时目录夹具。
use super::*;
use pierlink_core::terminal::{self, InputSource, TerminalEvent, TerminalManager, TerminalSpec};
use std::{sync::mpsc, time::Instant};

struct Terminal {
    manager: TerminalManager,
    id: String,
    events: mpsc::Receiver<TerminalEvent>,
}
impl Terminal {
    fn open(connections: &ConnectionManager, record: &Server, sshd: &support::Server) -> Self {
        let request = ConnectionRequest {
            server: record,
            class: ConnectionClass::Main,
            vault: Arc::new(PasswordVault::default()),
            callbacks: callbacks(&[(&record.id, sshd.options())]),
            interactive: true,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let kind = terminal::remote(connections, request, Some("terminal-test".into()), || {
            panic!("不能回退到 OpenSSH")
        })
        .unwrap();
        let manager = TerminalManager::new();
        let (tx, events) = mpsc::channel();
        let id = manager
            .open(
                TerminalSpec {
                    kind,
                    cols: 80,
                    rows: 24,
                },
                Arc::new(move |event| {
                    let _ = tx.send(event);
                }),
            )
            .unwrap();
        Self {
            manager,
            id,
            events,
        }
    }
    fn write(&self, bytes: &[u8]) {
        self.manager.write(&self.id, bytes).unwrap();
    }
    fn wait_output(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while !String::from_utf8_lossy(&output).contains(expected) {
            match self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                TerminalEvent::Output(bytes) => {
                    self.manager.ack(&self.id, bytes.len()).unwrap();
                    output.extend(bytes);
                }
                event => panic!("等待输出时意外结束：{event:?}"),
            }
        }
    }
    fn screen_contains(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self
                .manager
                .screen_text(&self.id)
                .unwrap()
                .contains(expected)
            {
                return;
            }
            assert!(Instant::now() < deadline, "屏幕缺少：{expected}");
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(TerminalEvent::Output(bytes)) => {
                    self.manager.ack(&self.id, bytes.len()).unwrap()
                }
                Ok(event) => panic!("读屏时意外结束：{event:?}"),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(error) => panic!("终端事件已关闭：{error}"),
            }
        }
    }
    fn exit(&self) -> (Option<i32>, bool) {
        loop {
            match self.events.recv_timeout(Duration::from_secs(10)).unwrap() {
                TerminalEvent::Output(bytes) => self.manager.ack(&self.id, bytes.len()).unwrap(),
                TerminalEvent::Exited {
                    code,
                    closed_by_user,
                } => return (code, closed_by_user),
            }
        }
    }
}

#[test]
#[ignore = "需要允许普通用户启动本机 sshd、监听随机端口，并安装 vim"]
fn 本机内置终端登录环境全屏程序尺寸与中断() {
    let sshd = support::Server::new("AcceptEnv LC_PIERLINK_TERM");
    let mut record = record("terminal", &sshd);
    record.login_shell = Some("/bin/sh".into());
    let connections = ConnectionManager::default();
    let terminal = Terminal::open(&connections, &record, &sshd);
    terminal.write(b"stty -echo; PS1=''; printf 'p%sl-ready\\n' ''\n");
    terminal.wait_output("pl-ready");
    terminal.write(b"printf '%s:%s\\n' \"$TERM\" \"$LC_PIERLINK_TERM\"\n");
    terminal.wait_output("xterm-256color:terminal-test");
    terminal.manager.resize(&terminal.id, 100, 40).unwrap();
    terminal.write(b"stty size\n");
    terminal.wait_output("40 100");
    terminal.write(
        b"vim -Nu NONE -i NONE -n -N -X -c 'set laststatus=2 ttimeout ttimeoutlen=10'; printf 'p%sl-vim-exited\\n' ''\n",
    );
    terminal.screen_contains("[No Name]");
    terminal.write(b"iscreen-one\nsecond-line\x1b");
    terminal.screen_contains("screen-one\nsecond-line");
    terminal.manager.resize(&terminal.id, 110, 35).unwrap();
    terminal.screen_contains("second-line");
    // 不加载配置时 vim 收到 Esc 后最多等 1 秒看是不是组合键，紧跟的 `:q!` 会被吞掉；上面已把这个等待设为 10 毫秒
    terminal.write(b":q!\r");
    // 由 shell 在 vim 退出后输出标记，避免后续输入被 vim 读走。
    terminal.wait_output("pl-vim-exited");
    terminal.write(b"printf 'p%sl-after-vim\\n' ''\n");
    terminal.wait_output("pl-after-vim");
    assert!(
        !terminal
            .manager
            .screen_text(&terminal.id)
            .unwrap()
            .contains("screen-one")
    );
    // 打出 running 的进程直接变成 sleep：负载高时 Ctrl+C 不会落在 shell 刚打完 running、还没启动 sleep 的空档里
    terminal.write(b"trap 'printf interrupted' INT; sh -c 'printf running; exec sleep 30'\n");
    terminal.wait_output("running");
    terminal.write(&[3]);
    terminal.wait_output("interrupted");
    terminal.write(b"exit 7\n");
    assert_eq!(terminal.exit(), (Some(7), false));
}

#[test]
#[ignore = "需要允许普通用户启动本机 sshd 并监听随机端口"]
fn 本机内置终端订阅输入协调与服务器断开() {
    let sshd = support::Server::new("AcceptEnv LC_PIERLINK_TERM");
    let mut record = record("terminal", &sshd);
    record.login_shell = Some("/bin/sh".into());
    let connections = ConnectionManager::default();
    let terminal = Terminal::open(&connections, &record, &sshd);
    terminal.write(b"stty -echo; PS1=''; printf 'p%sl-ready\\n' ''\n");
    terminal.wait_output("pl-ready");
    let fast = terminal.manager.subscribe(&terminal.id, 65536).unwrap();
    let slow = terminal.manager.subscribe(&terminal.id, 8).unwrap();
    terminal
        .manager
        .write_from(
            &terminal.id,
            b"printf automated\\n\n",
            InputSource::Automation,
        )
        .unwrap();
    assert!(
        terminal
            .events
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    terminal.wait_output("automated");
    let event = fast.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(!event.lost);
    assert!(matches!(event.event, TerminalEvent::Output(_)));
    assert!(slow.recv_timeout(Duration::ZERO).unwrap().lost);
    terminal
        .manager
        .write_from(
            &terminal.id,
            b"printf idle-automation\n",
            InputSource::Automation,
        )
        .unwrap();
    terminal.wait_output("idle-automation");
    terminal
        .manager
        .set_user_control(&terminal.id, true)
        .unwrap();
    assert!(
        terminal
            .manager
            .write_from(&terminal.id, b"refused", InputSource::Automation)
            .is_err()
    );
    terminal.write(b"printf user-control\n");
    terminal.wait_output("user-control");
    drop(sshd);
    // 服务器那头断开：退出码 255（和系统 ssh 一样），界面据此显示“连接已断开”
    assert_eq!(terminal.exit(), (Some(255), false));
}
