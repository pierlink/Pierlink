//! 仅改写路径字段的旧目录前缀；证书、私钥、索引和用户文件保持原字节。
use super::{Plan, exists, failure, io, plain};
use crate::error::AppError;
use quick_xml::{
    Reader, Writer,
    events::{BytesStart, BytesText, Event},
};
use rusqlite::{Connection, OpenFlags};
use std::{fs, io::Read, path::Path, time::Duration};

fn normalized(value: &str) -> String {
    let value = if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(value).to_owned()
    };
    value.replace('\\', "/")
}
fn equal(a: &str, b: &str) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
        let a: Vec<_> = a.encode_utf16().collect();
        let b: Vec<_> = b.encode_utf16().collect();
        unsafe {
            CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1)
                == CSTR_EQUAL
        }
    }
    #[cfg(not(windows))]
    {
        a.to_lowercase() == b.to_lowercase()
    }
}
fn path(value: &str, old: &Path, new: &Path) -> Option<String> {
    let candidate = normalized(value);
    let prefix = normalized(old.to_str()?).trim_end_matches('/').to_owned();
    let end = candidate
        .char_indices()
        .nth(prefix.chars().count())
        .map_or(candidate.len(), |(i, _)| i);
    if !equal(&candidate[..end], &prefix) {
        return None;
    }
    let suffix = &candidate[end..];
    if !suffix.is_empty() && !suffix.starts_with('/') {
        return None;
    }
    if suffix.split('/').any(|p| p == "..") {
        return None;
    }
    // 安装文件仍在旧位置，不能改写程序本身的引用。
    let first = suffix
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
    if matches!(
        first.to_ascii_lowercase().as_str(),
        "pierlink.exe" | "uninstall.exe" | "openssh" | "licenses"
    ) || (first.eq_ignore_ascii_case("syncthing")
        && suffix
            .trim_start_matches('/')
            .split_once('/')
            .is_some_and(|(_, n)| super::installed(n)))
    {
        return None;
    }
    let base = new.to_str()?.trim_end_matches(['/', '\\']);
    let suffix = if base.contains('\\') {
        suffix.replace('/', "\\")
    } else {
        suffix.into()
    };
    Some(format!("{base}{suffix}"))
}
fn json(value: &mut serde_json::Value, old: &Path, new: &Path) -> usize {
    match value {
        serde_json::Value::String(s) => {
            if let Some(updated) = path(s, old, new) {
                *s = updated;
                1
            } else {
                0
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().map(|v| json(v, old, new)).sum(),
        serde_json::Value::Object(o) => o.values_mut().map(|v| json(v, old, new)).sum(),
        _ => 0,
    }
}
fn xml(input: &[u8], old: &Path, new: &Path) -> Result<(Vec<u8>, usize), AppError> {
    let mut reader = Reader::from_reader(input);
    let mut writer = Writer::new(Vec::new());
    let mut changed = 0;
    let invalid = || failure("无法解析待迁移的 XML 配置");
    let mut depth = 0usize;
    let mut roots = 0;
    let mut pending = Vec::new();
    let mut decoded = String::new();
    loop {
        let event = reader.read_event().map_err(|_| invalid())?;
        match &event {
            Event::Text(t) => {
                decoded.push_str(t.as_ref());
                pending.push(event.into_owned());
                continue;
            }
            Event::CData(t) => {
                decoded.push_str(t.as_ref());
                pending.push(event.into_owned());
                continue;
            }
            Event::GeneralRef(r) => {
                let entity = format!("&{};", r.as_ref());
                decoded.push_str(&quick_xml::escape::unescape(&entity).map_err(|_| invalid())?);
                pending.push(event.into_owned());
                continue;
            }
            _ => {}
        }
        if let Some(updated) = path(decoded.trim(), old, new) {
            let start = decoded.len() - decoded.trim_start().len();
            let end = decoded.trim_end().len();
            let updated = format!("{}{}{}", &decoded[..start], updated, &decoded[end..]);
            writer
                .write_event(Event::Text(BytesText::new(&updated)))
                .map_err(|_| invalid())?;
            changed += 1;
        } else {
            for e in pending.drain(..) {
                writer.write_event(e).map_err(|_| invalid())?;
            }
        }
        pending.clear();
        decoded.clear();
        let event = match event {
            Event::DocType(_) => return Err(invalid()),
            Event::Start(ref s) | Event::Empty(ref s) => {
                if depth == 0 {
                    roots += 1;
                }
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                }
                if depth > 128 {
                    return Err(invalid());
                }
                let mut start = BytesStart::new(s.name().as_ref().to_owned());
                for a in s.attributes() {
                    let a = a.map_err(|_| invalid())?;
                    let val = a
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|_| invalid())?;
                    if let Some(updated) = path(&val, old, new) {
                        start.push_attribute((a.key.as_ref(), updated.as_str()));
                        changed += 1;
                    } else {
                        start.push_attribute((a.key.as_ref(), val.as_ref()));
                    }
                }
                if matches!(event, Event::Start(_)) {
                    Event::Start(start)
                } else {
                    Event::Empty(start)
                }
            }
            Event::End(end) => {
                depth = depth.checked_sub(1).ok_or_else(invalid)?;
                Event::End(end)
            }
            Event::Eof => break,
            other => other,
        };
        writer.write_event(event).map_err(|_| invalid())?;
    }
    if depth != 0 || roots != 1 {
        return Err(invalid());
    }
    Ok((writer.into_inner(), changed))
}
// 只解析 OpenSSH 的本机路径选项；保留注释、多个参数及 Windows 路径的反斜杠。
fn ssh(input: &str, old: &Path, new: &Path) -> (String, usize) {
    let mut changed = 0;
    let mut output = String::new();
    for line in input.split_inclusive('\n') {
        let trim = line.trim_start();
        let Some(split) = trim.find([' ', '\t', '=']) else {
            output.push_str(line);
            continue;
        };
        if !matches!(
            trim[..split].to_ascii_lowercase().as_str(),
            "identityfile"
                | "certificatefile"
                | "userknownhostsfile"
                | "globalknownhostsfile"
                | "include"
                | "controlpath"
        ) {
            output.push_str(line);
            continue;
        }
        let offset = line.len() - trim.len() + split;
        let mut chars = line[offset..].char_indices().peekable();
        let mut start = None;
        let mut quoted = false;
        let mut value = String::new();
        let mut edits = Vec::new();
        while let Some((i, c)) = chars.next() {
            if !quoted && (c.is_whitespace() || c == '#' || (c == '=' && start.is_none())) {
                if let Some(begin) = start.take() {
                    if let Some(updated) = path(&value.replace("%%", "%"), old, new) {
                        edits.push((begin, offset + i, updated));
                    }
                    value.clear();
                }
                if c == '#' {
                    break;
                }
                continue;
            }
            start.get_or_insert(offset + i);
            match c {
                '"' => quoted = !quoted,
                '\\' if chars.peek().is_some_and(|(_, c)| matches!(c, '"' | '\\')) => {
                    if let Some((_, c)) = chars.next() {
                        value.push(c);
                    }
                }
                _ => value.push(c),
            }
        }
        if !quoted
            && let Some(begin) = start
            && let Some(updated) = path(&value.replace("%%", "%"), old, new)
        {
            edits.push((begin, line.len(), updated));
        }
        let mut last = 0;
        for (start, end, updated) in edits {
            output.push_str(&line[last..start]);
            output.push('"');
            output.push_str(
                &updated
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('%', "%%"),
            );
            output.push('"');
            last = end;
            changed += 1;
        }
        output.push_str(&line[last..]);
    }
    (output, changed)
}
fn read(path: &Path) -> Result<Vec<u8>, AppError> {
    plain(path)?;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|f| f.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(io)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(failure("待迁移配置超过 16 MiB"));
    }
    Ok(bytes)
}
fn collect(
    root: &Path,
    relative: &str,
    output: &mut Vec<String>,
    depth: usize,
) -> Result<(), AppError> {
    if depth > 64 {
        return Err(failure("配置目录嵌套过深"));
    }
    let p = root.join(relative);
    plain(&p)?;
    if p.is_dir() {
        for entry in fs::read_dir(&p).map_err(io)? {
            let e = entry.map_err(io)?;
            let name = e
                .file_name()
                .into_string()
                .map_err(|_| failure("配置文件名不是有效文本"))?;
            collect(root, &format!("{relative}/{name}"), output, depth + 1)?;
        }
    } else if relative.starts_with("ssh-configs/")
        || relative == ".ssh/config"
        || matches!(
            p.extension().and_then(|s| s.to_str()),
            Some("json" | "xml" | "conf")
        )
    {
        output.push(relative.into());
    }
    Ok(())
}
pub(super) fn files(plan: &Plan) -> Result<Vec<String>, AppError> {
    let mut files = vec!["pierlink.db".into()];
    for relative in &plan.items {
        if matches!(
            relative.as_str(),
            "pierlink.db-wal" | "pierlink.db-shm" | "pierlink.db-journal"
        ) || relative.starts_with("syncthing/config.xml")
        {
            files.push(relative.clone());
        }
        if matches!(relative.as_str(), "config" | ".ssh" | "ssh-configs") {
            collect(&plan.old, relative, &mut files, 0)?;
        }
        if relative == "backups" {
            for entry in fs::read_dir(plan.old.join(relative)).map_err(io)? {
                let entry = entry.map_err(io)?;
                let name = entry.file_name();
                if let Some(name) = name
                    .to_str()
                    .filter(|n| n.starts_with("pierlink-") && n.ends_with(".db"))
                {
                    plain(&entry.path())?;
                    files.push(format!("backups/{name}"));
                }
            }
        }
    }
    files.sort();
    Ok(files)
}
fn sql_error(_: rusqlite::Error) -> AppError {
    failure("无法改写数据库中的本机路径")
}
fn database(file: &Path, old: &Path, new: &Path) -> Result<usize, AppError> {
    let mut conn =
        Connection::open_with_flags(file, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(sql_error)?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(sql_error)?;
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(sql_error)?;
    if check != "ok" {
        return Err(failure("待迁移数据库完整性检查失败"));
    }
    let tx = conn.transaction().map_err(sql_error)?;
    let mut changed = 0;
    for (table, column) in [
        ("servers", "identity_file"),
        ("sync_tasks", "local_path"),
        ("editors", "exe_path"),
    ] {
        let rows = tx
            .prepare(&format!(
                "SELECT rowid, {column} FROM {table} WHERE {column} IS NOT NULL"
            ))
            .map_err(sql_error)?
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for (rowid, value) in rows {
            if let Some(updated) = path(&value, old, new) {
                tx.execute(
                    &format!("UPDATE {table} SET {column}=?1 WHERE rowid=?2"),
                    rusqlite::params![updated, rowid],
                )
                .map_err(sql_error)?;
                changed += 1;
            }
        }
    }
    let mut json_columns = vec![("editors", "open_args"), ("editors", "diff_args")];
    // 备份可能来自旧版数据库，只有存在新列时才改写完整身份列表。
    let has_identity_files: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('servers') WHERE name='identity_files')",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if has_identity_files {
        json_columns.push(("servers", "identity_files"));
    }
    for (table, column) in json_columns {
        let rows = tx
            .prepare(&format!(
                "SELECT rowid,{column} FROM {table} WHERE {column} IS NOT NULL"
            ))
            .map_err(sql_error)?
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for (rowid, input) in rows {
            let mut value: serde_json::Value =
                serde_json::from_str(&input).map_err(|_| failure("数据库路径列表不是有效 JSON"))?;
            let count = json(&mut value, old, new);
            if count > 0 {
                tx.execute(
                    &format!("UPDATE {table} SET {column}=?1 WHERE rowid=?2"),
                    rusqlite::params![value.to_string(), rowid],
                )
                .map_err(sql_error)?;
                changed += count;
            }
        }
    }
    let rows = tx
        .prepare("SELECT key,value FROM settings")
        .map_err(sql_error)?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    for (key, value) in rows {
        let mut value: serde_json::Value = serde_json::from_str(&value)
            .map_err(|_| failure("数据库设置不是有效 JSON，无法安全迁移路径"))?;
        let count = json(&mut value, old, new);
        if count > 0 {
            tx.execute(
                "UPDATE settings SET value=?1 WHERE key=?2",
                rusqlite::params![value.to_string(), key],
            )
            .map_err(sql_error)?;
            changed += count;
        }
    }
    tx.commit().map_err(sql_error)?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .map_err(sql_error)?;
    // 确认数据库及 WAL 句柄已关闭，再刷盘并交给后续改名流程。
    conn.close().map_err(|(_, error)| sql_error(error))?;
    fs::OpenOptions::new()
        .write(true)
        .open(file)
        .and_then(|f| f.sync_all())
        .map_err(io)?;
    Ok(changed)
}
pub(super) fn prepare(plan: &Plan, journal: &Path) -> Result<usize, AppError> {
    let prepared = journal.join("prepared");
    fs::create_dir(&prepared).map_err(io)?;
    // 使用数据库及其 WAL 的私有副本，绝不打开原库，避免检查点写入原文件。
    for name in ["pierlink.db", "pierlink.db-wal", "pierlink.db-journal"] {
        let from = plan.old.join(name);
        if exists(&from)? {
            fs::copy(&from, prepared.join(name)).map_err(io)?;
        }
    }
    let mut changed = database(&prepared.join("pierlink.db"), &plan.old, &plan.new)?;
    for name in &plan.replacements {
        if name.starts_with("pierlink.db") {
            continue;
        }
        let from = plan.old.join(name);
        if name.starts_with("backups/") {
            let to = prepared.join(name);
            fs::create_dir_all(to.parent().ok_or_else(|| failure("数据库备份没有父目录"))?)
                .map_err(io)?;
            fs::copy(&from, &to).map_err(io)?;
            changed += database(&to, &plan.old, &plan.new)?;
            continue;
        }
        let bytes = read(&from)?;
        let (updated, count) = if name.starts_with("syncthing/config.xml")
            || from.extension().is_some_and(|s| s == "xml")
        {
            xml(&bytes, &plan.old, &plan.new)?
        } else if from.extension().is_some_and(|s| s == "json") {
            let mut v: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| failure("配置文件不是有效 JSON"))?;
            let count = json(&mut v, &plan.old, &plan.new);
            (
                serde_json::to_vec_pretty(&v).map_err(|_| failure("无法编码迁移后配置"))?,
                count,
            )
        } else {
            let input =
                std::str::from_utf8(&bytes).map_err(|_| failure("SSH 配置不是有效 UTF-8 文本"))?;
            let (out, count) = ssh(input, &plan.old, &plan.new);
            (out.into_bytes(), count)
        };
        let to = prepared.join(name);
        fs::create_dir_all(to.parent().ok_or_else(|| failure("配置文件没有父目录"))?)
            .map_err(io)?;
        let updated = if count == 0 { &bytes } else { &updated };
        crate::syncthing::home::atomic_write(&to, updated)?;
        fs::set_permissions(&to, fs::metadata(&from).map_err(io)?.permissions()).map_err(io)?;
        changed += count;
    }
    // 日志、SHM、回滚日志均由 SQLite 重建，旧文件在提交前仍由 original-* 保管。
    for suffix in ["-wal", "-shm", "-journal"] {
        let name = prepared.join(format!("pierlink.db{suffix}"));
        if exists(&name)? {
            fs::remove_file(name).map_err(io)?;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows路径前缀大小写边界及程序引用() {
        let old = Path::new(r"C:\Users\开发者\Pierlink");
        let new = Path::new(r"C:\Users\开发者\com.pierlink.desktop");
        assert_eq!(
            path(r"c:/users/开发者/PIERLINK/keys/a", old, new).unwrap(),
            r"C:\Users\开发者\com.pierlink.desktop\keys\a"
        );
        for value in [
            r"C:\Users\开发者\Pierlink2\a",
            r"prefix C:\Users\开发者\Pierlink\a",
            r"C:\Users\开发者\Pierlink\..\other",
            r"C:\Users\开发者\Pierlink\Pierlink.exe",
            r"C:\Users\开发者\Pierlink\syncthing\syncthing.exe",
        ] {
            assert!(path(value, old, new).is_none());
        }
    }
    #[test]
    fn xml实体转义属性文本及ssh路径() {
        let old = Path::new("/old & data");
        let new = Path::new("/new & data");
        let (out, count) = xml(br#"<configuration><folder path="/old &amp; data/files"><versioning><fsPath>/old &amp; data/versions</fsPath></versioning></folder></configuration>"#, old, new).unwrap();
        assert_eq!(count, 2);
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("/new &amp; data/versions")
        );
        let (out, count) = ssh(
            " IdentityFile \"/old & data/keys/a\"\nHostName other\n",
            old,
            new,
        );
        assert_eq!(count, 1);
        assert!(out.contains("\"/new & data/keys/a\""));
        let (out, count) = ssh(
            "UserKnownHostsFile \"/old & data/.ssh/hosts\" \"/elsewhere\" # /old & data/保留注释\r\n",
            old,
            new,
        );
        assert_eq!(count, 1);
        assert_eq!(
            out,
            "UserKnownHostsFile \"/new & data/.ssh/hosts\" \"/elsewhere\" # /old & data/保留注释\r\n"
        );
    }
}
