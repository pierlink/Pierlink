//! 按 UTC 日期追加审计，只清理本模块命名的过期日志。
use super::{AuditRecord, lock};
use crate::{error::AppError, security::redact};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

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
