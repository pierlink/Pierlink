use super::*;

// 首批八个请求收齐才响应；后续批次逆序返回，短块模式强制补读中间缺口。
const SERVER: &str = r#"
import os, select, struct, sys
r = sys.stdin.fileno()
w = sys.stdout.buffer
pending = []
first = True
data = bytearray()
def exact(n):
    result = b''
    while len(result) < n:
        chunk = os.read(r, n - len(result))
        if not chunk:
            raise EOFError()
        result += chunk
    return result
def read_packet():
    return exact(struct.unpack('>I', exact(4))[0])
def send(p):
    w.write(struct.pack('>I', len(p)) + p)
    w.flush()
def status(i, code=0):
    return struct.pack('>BIIII', 101, i, code, 0, 0)
def flush():
    global first
    replies = []
    for p in pending:
        kind, i = struct.unpack('>BI', p[:5])
        hlen = struct.unpack('>I', p[5:9])[0]
        offset = struct.unpack('>Q', p[9+hlen:17+hlen])[0]
        n = struct.unpack('>I', p[17+hlen:21+hlen])[0]
        if MODE == 'timeout' and offset == 0:
            continue
        if MODE == 'unknown':
            i += 1000
        if kind == 6:
            if MODE == 'failure' and offset == 32768:
                reply = status(i, 4)
            else:
                if len(data) < offset + n:
                    data.extend(bytes(offset + n - len(data)))
                data[offset:offset+n] = p[21+hlen:21+hlen+n]
                reply = status(i)
        else:
            chunk = data[offset:offset+min(n, 4093 if MODE == 'short' else n)]
            reply = struct.pack('>BII', 103, i, len(chunk)) + chunk if chunk else status(i, 1)
            if MODE == 'eof_failure' and offset == 32768:
                reply = status(i, 4)
        replies.append(reply)
    for reply in reversed(replies):
        send(reply)
        if MODE == 'duplicate':
            send(reply)
    pending.clear()
    first = False
read_packet()
send(struct.pack('>BI', 2, 3))
try:
    while True:
        if pending and not first and not select.select([r], [], [], 0.005)[0]:
            flush()
        p = read_packet()
        kind, i = struct.unpack('>BI', p[:5])
        if kind in (5, 6):
            pending.append(p)
            if len(pending) == 8:
                flush()
        elif kind == 3:
            first = True
            send(struct.pack('>BII', 102, i, 1) + b'h')
        elif kind in (7, 17):
            send(struct.pack('>BII', 105, i, 0))
        else:
            send(status(i))
except (EOFError, BrokenPipeError):
    pass
"#;
fn fake(mode: &str) -> SftpSession {
    crate::sftp::sshd_tests::open(
        &SshProgram {
            path: "/usr/bin/python3".into(),
        },
        &SshCommand {
            args: vec!["-c".into(), format!("MODE = '{mode}'\n{SERVER}")],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        },
        if mode == "timeout" {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(2)
        },
    )
    .unwrap()
}
#[test]
fn 八请求确实同时发出且逆序响应短块补读不丢数据() {
    let mut session = fake("short");
    session.next_id = u32::MAX - 3;
    let bytes: Vec<_> = (0..600_007).map(|i| (i % 251) as u8).collect();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        session
            .write_from("file", &mut &bytes[..], None, &mut |_| {}, &cancel)
            .unwrap(),
        bytes.len() as u64
    );
    assert_eq!(session.transport.max_pending, 8);
    session.transport.max_pending = 0;
    let mut output = vec![];
    assert_eq!(
        session
            .read_to("file", &mut output, &mut |_| {}, &cancel)
            .unwrap(),
        bytes.len() as u64
    );
    assert_eq!(output, bytes);
    assert_eq!(session.transport.max_pending, 8);
    assert!(session.stat(".").unwrap().is_some());
}
#[test]
fn 块失败后排空窗口再关闭句柄且可继续请求() {
    let mut session = fake("failure");
    let mut counts = vec![];
    assert_eq!(
        session
            .write_from(
                "file",
                &mut &vec![9; CHUNK * 10][..],
                None,
                &mut |n| counts.push(n),
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .code,
        "sftp.failure"
    );
    assert_eq!(counts, [CHUNK as u64]);
    assert!(session.stat(".").unwrap().is_some());
}
#[test]
fn 乱序窗口拒绝未知或重复编号并中止连接() {
    for mode in ["unknown", "duplicate"] {
        let mut session = fake(mode);
        assert_eq!(
            session
                .write_from(
                    "file",
                    &mut &vec![9; CHUNK * 8][..],
                    None,
                    &mut |_| {},
                    &AtomicBool::new(false)
                )
                .unwrap_err()
                .code,
            "sftp.protocol"
        );
        assert_eq!(session.stat(".").unwrap_err().code, "sftp.connection_lost");
    }
}

#[test]
fn 末尾预读块失败不能被当成成功() {
    let mut session = fake("eof_failure");
    assert_eq!(
        session
            .read_to("file", &mut vec![], &mut |_| {}, &AtomicBool::new(false))
            .unwrap_err()
            .code,
        "sftp.failure"
    );
    assert!(session.stat(".").unwrap().is_some());
}

#[test]
fn 最早请求缺失时后续响应不能延长超时() {
    let mut session = fake("timeout");
    let start = std::time::Instant::now();
    assert_eq!(
        session
            .write_from(
                "file",
                &mut &vec![9; CHUNK * 8][..],
                None,
                &mut |_| {},
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .code,
        "sftp.connection_lost"
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(session.stat(".").unwrap_err().code, "sftp.connection_lost");
}
