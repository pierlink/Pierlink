//! 枚举用户 SSH 配置中的别名；实际参数始终交给 OpenSSH 解析。
use crate::error::AppError;
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub mod conversion;
mod resolve;
pub use resolve::{ImportCandidate, preview, resolve_alias};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigHost {
    pub alias: String,
    /// 定义所在的文件及行号（从 1 开始）。
    pub file: PathBuf,
    pub line: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    Wildcard,
    Negated,
    InMatchBlock,
    InvalidAlias,
    Duplicate,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub pattern: String,
    pub reason: SkipReason,
}
#[derive(Debug, Default)]
pub struct HostList {
    pub hosts: Vec<ConfigHost>,
    pub skipped: Vec<Skipped>,
    pub warnings: Vec<AppError>,
}

pub(crate) fn validate_alias(alias: &str) -> Result<(), AppError> {
    if alias.is_empty()
        || alias.len() > 255
        || alias.starts_with(['-', '!'])
        || alias
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "%*?".contains(c))
    {
        return Err(AppError::new("ssh.invalid_alias").with("reason", "别名长度或字符不合法"));
    }
    Ok(())
}
fn too_large() -> AppError {
    AppError::new("ssh.config_too_large").with(
        "reason",
        "配置超过文件大小、文件数量、别名数量或包含深度限制",
    )
}

/// 枚举 config（不存在时为空），home 指定用户目录；返回别名、跳过项和读取警告。
/// 每文件最多 1 MiB，最多读取 64 个文件、5000 个别名，Include 最深 16 层。
pub fn list_hosts(config: &Path, home: &Path) -> Result<HostList, AppError> {
    let mut parser = Parser {
        home,
        list: HostList::default(),
        aliases: HashSet::new(),
        stack: HashSet::new(),
        files: 0,
    };
    parser.read(config, 0)?;
    Ok(parser.list)
}
struct Parser<'a> {
    home: &'a Path,
    list: HostList,
    aliases: HashSet<String>,
    stack: HashSet<PathBuf>,
    files: usize,
}
impl Parser<'_> {
    fn skip(&mut self, pattern: &str, reason: SkipReason) {
        self.list.skipped.push(Skipped {
            pattern: pattern.chars().take(64).collect(),
            reason,
        });
    }
    fn warning(&mut self, path: &Path, code: &str) {
        self.list.warnings.push(
            AppError::new(code)
                .with(
                    "file",
                    crate::security::redact(&path.to_string_lossy(), &[]),
                )
                .with("reason", "无法读取配置或检测到循环包含"),
        );
    }
    fn read(&mut self, path: &Path, depth: usize) -> Result<(), AppError> {
        let canonical = match fs::canonicalize(path) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {
                self.warning(path, "ssh.config_read_failed");
                return Ok(());
            }
        };
        if self.stack.contains(&canonical) {
            self.warning(path, "ssh.config_include_cycle");
            return Ok(());
        }
        if depth > 16 || self.files >= 64 {
            return Err(too_large());
        }
        self.files += 1;
        if !fs::metadata(path).is_ok_and(|m| m.is_file()) {
            self.warning(path, "ssh.config_read_failed");
            return Ok(());
        }
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => {
                self.warning(path, "ssh.config_read_failed");
                return Ok(());
            }
        };
        if !file.metadata().is_ok_and(|m| m.is_file()) {
            self.warning(path, "ssh.config_read_failed");
            return Ok(());
        }
        let mut bytes = Vec::new();
        if file.take(1024 * 1024 + 1).read_to_end(&mut bytes).is_err() {
            self.warning(path, "ssh.config_read_failed");
            return Ok(());
        }
        if bytes.len() > 1024 * 1024 {
            return Err(too_large());
        }
        self.stack.insert(canonical.clone());
        for (line, text) in String::from_utf8_lossy(&bytes)
            .trim_start_matches('\u{feff}')
            .lines()
            .enumerate()
        {
            let text = text.trim_start();
            let end = text
                .find(|c: char| c.is_whitespace() || c == '=')
                .unwrap_or(text.len());
            let key = &text[..end];
            let values = text[end..]
                .trim_start()
                .strip_prefix('=')
                .unwrap_or(text[end..].trim_start())
                .trim_start();
            match key.to_ascii_lowercase().as_str() {
                "host" => {
                    for alias in words(values) {
                        let reason = if alias.starts_with('!') {
                            Some(SkipReason::Negated)
                        } else if alias.contains(['*', '?']) {
                            Some(SkipReason::Wildcard)
                        } else if validate_alias(&alias).is_err() {
                            Some(SkipReason::InvalidAlias)
                        } else if !self.aliases.insert(alias.clone()) {
                            Some(SkipReason::Duplicate)
                        } else {
                            None
                        };
                        if let Some(reason) = reason {
                            self.skip(&alias, reason);
                        } else {
                            if self.list.hosts.len() >= 5000 {
                                return Err(too_large());
                            }
                            self.list.hosts.push(ConfigHost {
                                alias,
                                file: path.into(),
                                line: (line + 1) as u32,
                            });
                        }
                    }
                }
                "match" => self.skip(values, SkipReason::InMatchBlock),
                "include" => {
                    for pattern in words(values) {
                        let path = if pattern == "~" {
                            self.home.to_owned()
                        } else if let Some(rest) = pattern
                            .strip_prefix("~/")
                            .or_else(|| pattern.strip_prefix("~\\"))
                        {
                            self.home.join(rest)
                        } else if Path::new(&pattern).is_absolute() {
                            PathBuf::from(pattern)
                        } else {
                            self.home.join(".ssh").join(pattern)
                        };
                        for included in self.expand(&path)? {
                            self.read(&included, depth + 1)?;
                        }
                    }
                }
                _ => {}
            }
        }
        self.stack.remove(&canonical);
        Ok(())
    }
    fn expand(&mut self, pattern: &Path) -> Result<Vec<PathBuf>, AppError> {
        let mut paths = vec![PathBuf::new()];
        for component in pattern.components() {
            let name = component.as_os_str().to_string_lossy();
            if !name.contains(['*', '?']) {
                for path in &mut paths {
                    path.push(component);
                }
                continue;
            }
            let mut next = Vec::new();
            for parent in paths {
                let entries = match fs::read_dir(&parent) {
                    Ok(e) => e,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(_) => {
                        self.warning(&parent, "ssh.config_read_failed");
                        continue;
                    }
                };
                for entry in entries {
                    match entry {
                        Ok(entry)
                            if (name.starts_with('.')
                                || !entry.file_name().to_string_lossy().starts_with('.'))
                                && wildcard(&name, &entry.file_name().to_string_lossy()) =>
                        {
                            next.push(entry.path());
                            if next.len() > 64 {
                                return Err(too_large());
                            }
                        }
                        Err(_) => self.warning(&parent, "ssh.config_read_failed"),
                        _ => {}
                    }
                }
            }
            next.sort();
            paths = next;
        }
        Ok(paths)
    }
}
// 只对双引号分组，保留 Windows 路径中的普通反斜杠；注释在引号外开始。
fn words(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '#' if !quoted => break,
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '\\' if chars.peek().is_some_and(|c| matches!(c, '"' | '\\')) => {
                if let Some(c) = chars.next() {
                    word.push(c);
                    started = true;
                }
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    result.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        if quoted {
            word.push('\n');
        }
        result.push(word);
    }
    result
}
fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<_> = pattern.chars().collect();
    let t: Vec<_> = text.chars().collect();
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            i += 1;
            retry = j;
        } else if let Some(s) = star {
            retry += 1;
            j = retry;
            i = s + 1;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}

#[cfg(test)]
mod tests;
