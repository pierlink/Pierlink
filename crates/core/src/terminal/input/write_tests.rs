use super::*;
use std::io;

const NEWLINE_KEY: &[u8] = b"\x1b[13;28;13;1;16;1_";
const STOP: &[u8] = "测试写入结束".as_bytes();

#[test]
fn 识别完整和空参数的按键序列并排除其他控制序列() {
    for data in [
        NEWLINE_KEY,
        b"\x1b[_",
        b"\x1b[;;;;;_",
        b"\x1b[27;;27;1;;_",
        b"abc\x1b[13;28;13;1;16;1_xyz",
        b"\x1b[200~\x1b[A\x1b[_",
    ] {
        assert!(Win32Input::default().after(data) == Win32Input::Enabled);
    }
    for data in [
        b"".as_slice(),
        b"\x1b",
        b"\x1b[",
        b"\x1b[27;1;27;1;0;1",
        b"\x1b[200~",
        b"\x1b[A",
        b"\x1bx",
        b"[27;1;27;1;0;1_",
        b"\x1b[?1_",
        b"\x1b[1:2_",
        b"\x1b[1a_",
    ] {
        assert!(Win32Input::default().after(data) != Win32Input::Enabled);
    }
}

#[test]
fn 按键序列跨写入识别且启用后不再清除() {
    for split in 0..=NEWLINE_KEY.len() {
        let state = Win32Input::default()
            .after(&NEWLINE_KEY[..split])
            .after(&NEWLINE_KEY[split..]);
        assert!(state == Win32Input::Enabled);
        assert!(state.after(b"\x1bc\x1b[?9001l\x03") == Win32Input::Enabled);
    }
}

#[test]
fn 只转换已启用状态下的末尾一个转义字节() {
    for (data, prefix) in [
        (b"\x1b".as_slice(), b"".as_slice()),
        (b"abc\x1b", b"abc"),
        (b"\x1b\x1b", b"\x1b"),
        (b"\x1b[A\x1bx\x1b", b"\x1b[A\x1bx"),
    ] {
        assert_eq!(
            escape_trailing_esc(data, true).as_ref(),
            [prefix, ESC_KEY].concat()
        );
        assert_eq!(escape_trailing_esc(data, false).as_ref(), data);
    }
    for data in [b"".as_slice(), b"abc", b"\x1b[A", b"\x1bx", NEWLINE_KEY] {
        assert_eq!(escape_trailing_esc(data, true).as_ref(), data);
    }
}

struct Writer<F>(F);
impl<F: FnMut(&[u8]) -> io::Result<usize>> Write for Writer<F> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        (self.0)(data)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn queued(conpty: bool, source: InputSource, entries: &[&[u8]]) -> Input {
    let mut input = Input::new(conpty);
    for data in entries {
        match source {
            InputSource::User => input.user.push_back(data.to_vec()),
            InputSource::Automation => input.automation.push_back((data.to_vec(), None)),
        }
        input.bytes += data.len();
    }
    input
}

fn record(
    mut input: Input,
    max_write: usize,
    mut on_write: impl FnMut(&[u8], &Mutex<Input>),
) -> Vec<Vec<u8>> {
    input.automation.push_back((STOP.to_vec(), None));
    input.bytes += STOP.len();
    let input = Mutex::new(input);
    let flow = Mutex::new(Flow::default());
    let mut writes = Vec::new();
    let mut writer = Writer(|data: &[u8]| {
        if data == STOP {
            // 最后一条原始输入之外的容量应已全部归还，包括被转换或丢弃的数据。
            assert_eq!(lock(&input).bytes, STOP.len());
            lock(&flow).stopped = true;
            return Ok(data.len());
        }
        let count = data.len().min(max_write);
        writes.push(data[..count].to_vec());
        on_write(&data[..count], &input);
        Ok(count)
    });
    write_queued_input(&mut writer, &flow, &input, &Condvar::new());
    let input = lock(&input);
    assert_eq!(input.bytes, 0);
    assert!(!input.failed);
    assert!(input.user.is_empty() && input.automation.is_empty());
    writes
}

#[test]
fn 会话只在启用标志且写过按键序列后转换用户和自动化输入() {
    for conpty in [false, true] {
        for source in [InputSource::User, InputSource::Automation] {
            let input = queued(
                conpty,
                source,
                &[b"\x1b", NEWLINE_KEY, b"\x1b", b"\x03", b"\x1b"],
            );
            let writes = record(input, usize::MAX, |_, _| {});
            let esc = if conpty { ESC_KEY } else { b"\x1b" };
            assert_eq!(
                writes.concat(),
                [b"\x1b", NEWLINE_KEY, esc, b"\x03", esc].concat()
            );
        }
    }
}

#[test]
fn 同条输入中的按键序列生效且系统短写不丢失字节() {
    let data = [NEWLINE_KEY, b"abc\x1b"].concat();
    for max_write in [1, 7, usize::MAX] {
        let input = queued(true, InputSource::Automation, &[&data]);
        let writes = record(input, max_write, |_, _| {});
        assert_eq!(writes.concat(), [NEWLINE_KEY, b"abc", ESC_KEY].concat());
    }
}

#[test]
fn 分块末尾的转义字节不转换且跨块按键序列能启用状态() {
    let mut data = vec![b'x'; 4095];
    data.extend_from_slice(NEWLINE_KEY);
    data.resize(8191, b'x');
    data.extend_from_slice(b"\x1b[A\x1bx\x1b");
    let input = queued(true, InputSource::Automation, &[&data]);
    let writes = record(input, usize::MAX, |_, _| {});
    assert_eq!(writes[0].len(), 4096);
    assert_eq!(writes[0].last(), Some(&0x1b));
    assert_eq!(writes[1].len(), 4096);
    assert_eq!(writes[1].last(), Some(&0x1b));
    assert!(writes.iter().all(|chunk| chunk.len() <= 4096));
    assert_eq!(writes.concat(), [&data[..data.len() - 1], ESC_KEY].concat());
}

#[test]
fn 暂停重排队保留原始末尾和容量并在恢复时转换一次() {
    let mut data = vec![b'x'; 4096];
    data.push(0x1b);
    let input = queued(true, InputSource::Automation, &[NEWLINE_KEY, &data]);
    let mut paused = false;
    let mut resumed = false;
    let writes = record(input, usize::MAX, |chunk, input| {
        let mut input = lock(input);
        if chunk.len() == 4096 {
            assert_eq!(input.bytes, data.len() + STOP.len());
            input.user_at = Some(Instant::now());
            input.user.push_back(b"u".to_vec());
            input.bytes += 1;
            paused = true;
        } else if chunk == b"u" {
            assert_eq!(input.automation.front().unwrap().0, b"\x1b");
            assert_eq!(input.bytes, 2 + STOP.len());
            input.user_at = None;
            resumed = true;
        }
    });
    assert!(paused && resumed);
    assert_eq!(
        writes.concat(),
        [NEWLINE_KEY, &data[..4096], b"u", ESC_KEY].concat()
    );
}

#[test]
fn 被丢弃的按键序列不会提前启用转换() {
    let mut input = queued(true, InputSource::Automation, &[NEWLINE_KEY, b"\x1b"]);
    input.generation = 1;
    input.automation.front_mut().unwrap().1 = Some(0);
    let writes = record(input, usize::MAX, |_, _| {});
    assert_eq!(writes.concat(), b"\x1b");
}

#[test]
fn 自动化中途被用户打断时未写出的按键序列不启用转换() {
    let mut data = vec![b'x'; 4096];
    data.extend_from_slice(NEWLINE_KEY);
    data.push(0x1b);
    let input = queued(true, InputSource::Automation, &[&data]);
    let writes = record(input, usize::MAX, |chunk, input| {
        if chunk.len() == 4096 {
            let mut input = lock(input);
            input.generation += 1;
            input.user.push_back(b"\x1b".to_vec());
            input.bytes += 1;
        }
    });
    assert_eq!(writes.concat(), [&data[..4096], b"\x1b"].concat());
}

#[test]
fn 短写期间先发完一个转义按键再处理用户输入() {
    let input = queued(true, InputSource::Automation, &[NEWLINE_KEY, b"\x1b"]);
    let mut count = 0;
    let writes = record(input, 1, |_, input| {
        count += 1;
        let mut input = lock(input);
        if count == NEWLINE_KEY.len() + 1 {
            input.user.push_back(b"u".to_vec());
            input.bytes += 1;
            input.user_at = Some(Instant::now());
        } else if count == NEWLINE_KEY.len() + ESC_KEY.len() + 1 {
            input.user_at = None;
        }
    });
    assert_eq!(writes.concat(), [NEWLINE_KEY, ESC_KEY, b"u"].concat());
}

#[test]
fn 转义按键写入失败时清空原始容量() {
    for error_kind in [None, Some(io::ErrorKind::BrokenPipe)] {
        let input = Mutex::new(queued(
            true,
            InputSource::Automation,
            &[NEWLINE_KEY, b"\x1b", b"later"],
        ));
        let mut writer = Writer(|data: &[u8]| {
            if data == NEWLINE_KEY {
                Ok(data.len())
            } else if let Some(kind) = error_kind {
                Err(io::Error::from(kind))
            } else {
                Ok(0)
            }
        });
        write_queued_input(
            &mut writer,
            &Mutex::new(Flow::default()),
            &input,
            &Condvar::new(),
        );
        let input = lock(&input);
        assert!(input.failed);
        assert_eq!(input.bytes, 0);
        assert!(input.automation.is_empty());
    }
}
