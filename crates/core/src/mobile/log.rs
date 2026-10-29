//! 手机操作记录：按 UTC 日期追加到 `<目录>/YYYY-MM-DD.jsonl`，保留指定天数。
use crate::mcp::audit::{date, now_ms, prune_daily};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

/// 手机操作记录文件；每天第一次写入时清理过期文件。
pub struct MobileLog {
    dir: PathBuf,
    retain_days: u32,
    last_day: Mutex<Option<u64>>,
}

impl MobileLog {
    /// 在 dir 保存记录，保留 retain_days 天；目录在第一次写入时创建。
    pub fn new(dir: PathBuf, retain_days: u32) -> Self {
        Self {
            dir,
            retain_days,
            last_day: Mutex::new(None),
        }
    }

    /// 追加一行记录（自动加上毫秒时间 at）；写失败只在标准错误里说明，不影响操作本身。
    ///
    /// :param entry: 一个 JSON 对象，调用方保证不含邀请码、Cookie、输入文字等秘密
    pub fn write(&self, entry: Value) {
        let at = now_ms();
        let day = at / 86_400_000;
        {
            let mut last = super::lock(&self.last_day);
            if *last != Some(day) {
                if let Err(e) = prune_daily(&self.dir, self.retain_days) {
                    eprintln!("清理手机操作记录失败: 错误={:?}", e.kind());
                }
                *last = Some(day);
            }
        }
        let mut line = json!({ "at": at });
        if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), entry) {
            line.extend(fields);
        }
        let result = (|| -> std::io::Result<()> {
            fs::create_dir_all(&self.dir)?;
            let mut data = serde_json::to_vec(&line)?;
            data.push(b'\n');
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(format!("{}.jsonl", date(day))))?
                .write_all(&data)
        })();
        if let Err(e) = result {
            eprintln!("写手机操作记录失败: 错误={:?}", e.kind());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 追加记录并清理过期文件() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("2000-01-01.jsonl"), "old\n").unwrap();
        fs::write(dir.path().join("note.txt"), "keep").unwrap();
        let log = MobileLog::new(dir.path().to_path_buf(), 30);
        log.write(json!({ "action": "pair", "result": "ok" }));
        log.write(json!({ "action": "stop_all", "result": "ok" }));
        assert!(!dir.path().join("2000-01-01.jsonl").exists());
        assert!(dir.path().join("note.txt").exists());
        let today = date(now_ms() / 86_400_000);
        let text = fs::read_to_string(dir.path().join(format!("{today}.jsonl"))).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["action"], "pair");
        assert!(lines[1]["at"].as_u64().unwrap() > 0);
    }
}
