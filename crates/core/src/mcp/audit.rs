//! 按 UTC 日期追加审计，只清理本模块命名的过期日志。
use super::{AuditRecord, lock};
use crate::{error::AppError, security::redact};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// 读回最近的记录时，每个日志文件最多从末尾读这么多字节（一天的操作很多时不整个读进内存）
const RECENT_TAIL_BYTES: u64 = 4 * 1024 * 1024;

pub struct Audit {
    dir: PathBuf,
    retain_days: u32,
    last_day: Mutex<Option<u64>>,
}
impl Audit {
    /// 在 dir 保存审计，保留 retain_days 天；返回惰性创建目录的记录器。
    pub fn new(dir: PathBuf, retain_days: u32) -> Self {
        Self {
            dir,
            retain_days,
            last_day: Mutex::new(None),
        }
    }
    /// 向今天的 UTC 日志追加脱敏后的 record；失败只记录不含路径和内容的诊断。
    pub fn record(&self, record: &AuditRecord) {
        let day = now_ms() / 86_400_000;
        let mut last = lock(&self.last_day);
        if *last != Some(day) {
            if self.prune().is_err() {
                eprintln!("MCP 审计日志清理失败");
            }
            *last = Some(day);
        }
        let result = (|| -> Result<(), std::io::Error> {
            fs::create_dir_all(&self.dir)?;
            let mut record = record.clone();
            record.output_preview = None;
            sanitize_record(&mut record, &[]);
            let mut data = serde_json::to_vec(&record)?;
            data.push(b'\n');
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(format!("{}.jsonl", date(day))))?;
            file.write_all(&data)
        })();
        if let Err(error) = result {
            eprintln!("MCP 审计日志写入失败：{:?}", error.kind());
        }
    }
    /// 从最新的日志往回读最近 limit 条记录，新的在前（程序重启、更新后操作记录接着显示）。
    /// 每个文件只读末尾 RECENT_TAIL_BYTES；读不懂的行跳过；目录不存在或读不了时返回已读到的。
    pub fn recent(&self, limit: usize) -> Vec<AuditRecord> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return out;
        };
        let mut days: Vec<String> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let day = name.strip_suffix(".jsonl")?;
                valid_date(day).then(|| day.to_owned())
            })
            .collect();
        days.sort_unstable_by(|a, b| b.cmp(a));
        for day in days {
            let Ok(text) = read_tail(&self.dir.join(format!("{day}.jsonl")), RECENT_TAIL_BYTES)
            else {
                continue;
            };
            for line in text.lines().rev() {
                if out.len() >= limit {
                    return out;
                }
                if let Ok(record) = serde_json::from_str::<AuditRecord>(line) {
                    out.push(record);
                }
            }
        }
        out
    }
    /// 删除文件名日期早于保留期限的普通日志文件，返回删除数量；不跟随符号链接。
    pub fn prune(&self) -> Result<usize, AppError> {
        prune_daily(&self.dir, self.retain_days).map_err(|_| audit_error())
    }
}

/// 删除 dir 里文件名为 `YYYY-MM-DD.jsonl` 且早于 retain_days 天的普通文件，返回删除数量；
/// 目录不存在返回 0；不跟随符号链接，不碰其他文件。
pub(crate) fn prune_daily(dir: &std::path::Path, retain_days: u32) -> std::io::Result<usize> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let cutoff = date((now_ms() / 86_400_000).saturating_sub(retain_days as u64));
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(day) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if valid_date(day) && day < cutoff.as_str() && entry.file_type()?.is_file() {
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(removed)
}
/// 读文件末尾至多 max 字节的文字；不是从头读的时候丢掉第一行（多半只有半行）
fn read_tail(path: &std::path::Path, max: u64) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        let cut = text.find('\n').map_or(text.len(), |i| i + 1);
        text.drain(..cut);
    }
    Ok(text)
}
fn audit_error() -> AppError {
    AppError::new("mcp.audit_failed")
        .with_reason("mcp.couldn_t_clean_up_audit_logs", "无法清理审计日志")
}
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn leap(year: u64) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}
fn month_days(year: u64, month: usize) -> u64 {
    match month {
        2 if leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}
pub(crate) fn date(mut days: u64) -> String {
    let mut year = 1970;
    while days >= if leap(year) { 366 } else { 365 } {
        days -= if leap(year) { 366 } else { 365 };
        year += 1;
    }
    let mut month = 1;
    while days >= month_days(year, month) {
        days -= month_days(year, month);
        month += 1;
    }
    format!("{year:04}-{month:02}-{:02}", days + 1)
}
fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(i, b)| i != 4 && i != 7 && !b.is_ascii_digit())
    {
        return false;
    }
    let year: u64 = value[..4].parse().unwrap_or_default();
    let month: usize = value[5..7].parse().unwrap_or_default();
    let day: u64 = value[8..].parse().unwrap_or_default();
    year > 0 && (1..=12).contains(&month) && (1..=month_days(year, month)).contains(&day)
}
pub(super) fn sanitize_record(record: &mut AuditRecord, secrets: &[&str]) {
    record.session = redact(&record.session, secrets);
    record.client.name = redact(&record.client.name, secrets);
    record.client.version = redact(&record.client.version, secrets);
    if let super::Entry::Server { server_id } = &mut record.entry {
        *server_id = redact(server_id, secrets);
    }
    record.tool = redact(&record.tool, secrets);
    record.target = record.target.as_ref().map(|target| redact(target, secrets));
    record.summary = redact(&record.summary, secrets);
    record.outcome = redact(&record.outcome, secrets);
    record.approval = redact(&record.approval, secrets);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 日志保留期限与命名边界() {
        let dir = crate::test_support::tempdir();
        let today = now_ms() / 86_400_000;
        let old = format!("{}.jsonl", date(today - 31));
        let current = format!("{}.jsonl", date(today));
        let boundary = format!("{}.jsonl", date(today - 30));
        for name in [
            &old,
            &current,
            &boundary,
            "other.log",
            "2020-02-30.jsonl",
            "2020-1-01.jsonl",
        ] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        fs::create_dir(dir.path().join("2020-01-01.jsonl")).unwrap();
        let audit = Audit::new(dir.path().into(), 30);
        assert_eq!(audit.prune().unwrap(), 1);
        assert!(!dir.path().join(old).exists());
        for name in [
            &current,
            &boundary,
            "other.log",
            "2020-02-30.jsonl",
            "2020-1-01.jsonl",
            "2020-01-01.jsonl",
        ] {
            assert!(dir.path().join(name).exists());
        }
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(19782), "2024-02-29");
        assert!(valid_date("2000-02-29"));
        assert!(!valid_date("1900-02-29"));
        assert!(!valid_date("中文日期无效"));
    }
    #[test]
    fn 读回最近的记录_新的在前_跨天_跳过读不懂的行() {
        let dir = crate::test_support::tempdir();
        let today = now_ms() / 86_400_000;
        let line = |summary: &str| {
            serde_json::json!({
                "time_ms": 1, "session": "s", "entry": {"kind": "windows"},
                "client": {"name": "c", "version": "1"}, "tool": "run_command",
                "target": null, "summary": summary, "outcome": "ok",
                "exit_code": 0, "duration_ms": 1
            })
            .to_string()
        };
        fs::write(
            dir.path().join(format!("{}.jsonl", date(today - 1))),
            format!("{}\n{}\n", line("a"), line("b")),
        )
        .unwrap();
        fs::write(
            dir.path().join(format!("{}.jsonl", date(today))),
            format!("{}\n不是 JSON\n{}\n", line("c"), line("d")),
        )
        .unwrap();
        let audit = Audit::new(dir.path().into(), 30);
        let summaries = |n| {
            audit
                .recent(n)
                .into_iter()
                .map(|r| r.summary)
                .collect::<Vec<_>>()
        };
        assert_eq!(summaries(10), ["d", "c", "b", "a"]);
        assert_eq!(summaries(3), ["d", "c", "b"]);
        assert!(Audit::new(dir.path().join("没有"), 30).recent(5).is_empty());
        // 只读末尾时丢掉开头那半行
        let big = dir.path().join("big.jsonl");
        fs::write(&big, format!("{}\n{}\n", line("x"), line("y"))).unwrap();
        let tail = read_tail(&big, (line("y").len() + 5) as u64).unwrap();
        assert_eq!(tail.trim(), line("y"));
    }
    #[test]
    fn 追加记录与跨日清理及失败不影响调用() {
        let dir = crate::test_support::tempdir();
        let audit = Audit::new(dir.path().join("logs"), 30);
        assert_eq!(audit.prune().unwrap(), 0);
        let record = AuditRecord {
            output_preview: None,
            time_ms: now_ms(),
            session: "session".into(),
            entry: super::super::Entry::Windows,
            client: Default::default(),
            tool: "run_command".into(),
            target: None,
            summary: "password=秘密".into(),
            outcome: "ok".into(),
            approval: "user".into(),
            exit_code: Some(0),
            duration_ms: 1,
        };
        audit.record(&record);
        let path = audit
            .dir
            .join(format!("{}.jsonl", date(now_ms() / 86_400_000)));
        let text = fs::read_to_string(path).unwrap();
        assert!(!text.contains("秘密"));
        assert!(serde_json::from_str::<serde_json::Value>(text.trim()).is_ok());
        fs::write(audit.dir.join("2000-01-01.jsonl"), "").unwrap();
        *lock(&audit.last_day) = None;
        audit.record(&record);
        assert!(!audit.dir.join("2000-01-01.jsonl").exists());
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        let invalid = Audit::new(file, 30);
        invalid.record(&record);
        assert!(invalid.prune().is_err());
    }
}
