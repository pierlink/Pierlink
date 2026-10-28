//! 用法：bench host port user key SHA256:指纹 [MiB，默认256] [--system]
//! --system 额外测系统 ssh；该目标须预先存在于系统 known_hosts。
use pierlink_ssh_engine::{
    Authentication, Connection, ConnectionOptions, Engine, HostKeyCheck, Pty,
};
use std::{
    error::Error,
    io::{self, Read, Write},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 5 {
        return Err("用法：bench host port user key SHA256:指纹 [MiB] [--system]".into());
    }
    let mib: u64 = args
        .get(5)
        .filter(|value| *value != "--system")
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(256);
    let bytes = mib
        .checked_mul(1024 * 1024)
        .filter(|bytes| *bytes > 0)
        .ok_or("传输大小无效")?;
    let mut options = ConnectionOptions::new(
        &args[0],
        args[1].parse()?,
        &args[2],
        HostKeyCheck::Fingerprint(args[4].clone()),
    );
    options.io_timeout = Duration::from_secs(300);
    let engine = Engine::new()?;
    let started = Instant::now();
    let connection = engine.connect(
        options,
        Authentication::PrivateKey {
            path: args[3].clone().into(),
            passphrase: None,
        },
    )?;
    println!("内置：连接及认证 {:.3} 秒", started.elapsed().as_secs_f64());
    builtin(&connection, bytes)?;
    if args.iter().any(|value| value == "--system") {
        system(&args, bytes)?;
    }
    Ok(())
}

fn builtin(connection: &Connection, bytes: u64) -> Result<(), Box<dyn Error>> {
    let mut upload = connection.exec("printf READY; cat >/dev/null; printf DONE", &[])?;
    ready(&mut upload.stdout)?;
    let started = Instant::now();
    upload_bytes(&mut upload.stdin, bytes)?;
    upload.stdin.shutdown()?;
    let mut done = [0; 4];
    upload.stdout.read_exact(&mut done)?;
    if done != *b"DONE" || upload.wait()? != Some(0) {
        return Err("上传未成功完成".into());
    }
    rate("内置上传", bytes, started);
    let mut download = connection.exec(&format!("printf READY; head -c {bytes} /dev/zero"), &[])?;
    ready(&mut download.stdout)?;
    let started = Instant::now();
    download_bytes(&mut download.stdout, bytes)?;
    if download.wait()? != Some(0) {
        return Err("下载未成功完成".into());
    }
    rate("内置下载", bytes, started);
    let mut shell = connection.shell(&Pty::default(), &[])?;
    // 标记拆成两个字符串，防止命令行的 PTY 回显被误认为就绪标记。
    shell
        .stdin
        .write_all(b"stty raw -echo; printf '\\120\\114\\102\\105\\116\\103\\110'; cat\n")?;
    marker(&mut shell.stdout, b"PLBENCH")?;
    latency("内置 PTY", &mut shell.stdin, &mut shell.stdout)?;
    shell.close()?;
    println!(
        "内置累计换钥：{} 次（含首次握手）",
        connection.statistics().key_exchanges
    );
    Ok(())
}

fn upload_bytes(writer: &mut impl Write, mut bytes: u64) -> io::Result<()> {
    let block = vec![0x5a; 64 * 1024];
    while bytes > 0 {
        let count = bytes.min(block.len() as u64) as usize;
        writer.write_all(&block[..count])?;
        bytes -= count as u64;
    }
    Ok(())
}

fn download_bytes(reader: &mut impl Read, bytes: u64) -> io::Result<()> {
    let received = io::copy(reader, &mut io::sink())?;
    if received != bytes {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "下载字节数不符",
        ));
    }
    Ok(())
}

fn rate(label: &str, bytes: u64, started: Instant) {
    let seconds = started.elapsed().as_secs_f64();
    println!(
        "{label}：{:.2} MB/s，{bytes} 字节，{seconds:.3} 秒",
        bytes as f64 / 1_000_000.0 / seconds
    );
}

fn ready(reader: &mut impl Read) -> io::Result<()> {
    let mut buffer = [0; 5];
    reader.read_exact(&mut buffer)?;
    if buffer != *b"READY" {
        return Err(io::Error::other("服务器未正确发送就绪标记"));
    }
    Ok(())
}

fn marker(reader: &mut impl Read, marker: &[u8]) -> io::Result<()> {
    let mut matched = 0;
    for _ in 0..64 * 1024 {
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        if byte[0] == marker[matched] {
            matched += 1;
        } else {
            matched = usize::from(byte[0] == marker[0]);
        }
        if matched == marker.len() {
            return Ok(());
        }
    }
    Err(io::Error::other("等待终端就绪标记超限"))
}

fn latency(label: &str, writer: &mut impl Write, reader: &mut impl Read) -> io::Result<()> {
    let mut samples = Vec::with_capacity(100);
    for _ in 0..100 {
        let started = Instant::now();
        writer.write_all(b"x")?;
        writer.flush()?;
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        if byte != *b"x" {
            return Err(io::Error::other("PTY 回显内容不符"));
        }
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{label} 往返：均值 {:.3} ms，p50 {:.3} ms，p95 {:.3} ms，100 次",
        samples.iter().sum::<f64>() / 100.0,
        samples[49],
        samples[94]
    );
    Ok(())
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn ssh(args: &[String], command: &str, pty: bool) -> io::Result<Process> {
    let mut process = Command::new("ssh");
    process.args([
        "-F",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
        "-oBatchMode=yes",
        "-oIdentitiesOnly=yes",
        "-oStrictHostKeyChecking=yes",
        "-oCompression=no",
        "-oControlMaster=no",
        "-oLogLevel=ERROR",
        "-p",
        &args[1],
        "-l",
        &args[2],
        "-i",
        &args[3],
    ]);
    process.arg(if pty { "-tt" } else { "-T" });
    // 主机参数前加 --，不经过本地 shell；远程命令仅含本例的固定文本和已解析整数。
    process
        .args(["--", &args[0], command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    Ok(Process(process.spawn()?))
}

fn system(args: &[String], bytes: u64) -> Result<(), Box<dyn Error>> {
    let mut upload = ssh(args, "printf READY; cat >/dev/null; printf DONE", false)?;
    let mut input = upload.0.stdin.take().ok_or("没有标准输入")?;
    let mut output = upload.0.stdout.take().ok_or("没有标准输出")?;
    ready(&mut output)?;
    let started = Instant::now();
    upload_bytes(&mut input, bytes)?;
    drop(input);
    let mut done = [0; 4];
    output.read_exact(&mut done)?;
    if done != *b"DONE" || !upload.0.wait()?.success() {
        return Err("系统 ssh 上传失败".into());
    }
    rate("系统 ssh 上传", bytes, started);
    let mut download = ssh(
        args,
        &format!("printf READY; head -c {bytes} /dev/zero"),
        false,
    )?;
    let mut output = download.0.stdout.take().ok_or("没有标准输出")?;
    ready(&mut output)?;
    let started = Instant::now();
    download_bytes(&mut output, bytes)?;
    if !download.0.wait()?.success() {
        return Err("系统 ssh 下载失败".into());
    }
    rate("系统 ssh 下载", bytes, started);
    let mut shell = ssh(args, "stty raw -echo; printf PLBENCH; cat", true)?;
    let mut input = shell.0.stdin.take().ok_or("没有标准输入")?;
    let mut output = shell.0.stdout.take().ok_or("没有标准输出")?;
    marker(&mut output, b"PLBENCH")?;
    latency("系统 ssh PTY", &mut input, &mut output)?;
    Ok(())
}
