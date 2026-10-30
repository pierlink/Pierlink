//! 有界、可取消的本机文件检索；目录进入前复查授权，不跟随链接。
use super::{
    check_cancel,
    encoding::{Document, binary},
    files::{self, Sandbox},
    number, required, string,
};
use crate::error::AppError;
use globset::{GlobBuilder, GlobMatcher};
use rmcp::model::JsonObject;
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
const SCAN_LIMIT: usize = 20_000;
const FILE_LIMIT: usize = 10 * 1024 * 1024;
const OUTPUT_LIMIT: usize = 256 * 1024;
const SCAN_TIME: Duration = Duration::from_secs(10);

fn invalid(text_key: &'static str, reason: &str) -> AppError {
    AppError::new("mcp.invalid_argument").with_reason(text_key, reason)
}
fn glob(pattern: &str) -> Result<GlobMatcher, AppError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .case_insensitive(cfg!(windows))
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|_| invalid("mcp.invalid_glob_pattern", "glob 模式无效"))
}
/// 符号链接和目录联接（Windows 上 is_symlink 两种都算）。OneDrive 占位文件等其他重解析点不算链接：
/// 它们是普通文件和文件夹，越界与否由 resolve_read 的规范化路径判断。
fn link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}
fn safe_entry(sb: &Sandbox, path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    if sb.resolve_read(text).is_err() || fs::symlink_metadata(path).is_ok_and(|m| link(&m)) {
        return false;
    }

    true
}
struct Scan {
    paths: Vec<(PathBuf, PathBuf)>,
    truncated: bool,
}
fn collect(
    sb: &Sandbox,
    path: Option<&str>,
    ignored: bool,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<Scan, AppError> {
    let roots = if let Some(path) = path {
        vec![sb.resolve_read(path)?]
    } else {
        sb.folders.clone()
    };
    let mut result = Scan {
        paths: Vec::new(),
        truncated: false,
    };
    let mut seen = HashSet::new();
    let mut scanned = 0;
    for root in roots {
        check_cancel(cancelled)?;
        if !safe_entry(sb, &root) {
            return Err(invalid(
                "mcp.search_root_cannot_be_a_link_or_outside_the_allowed",
                "检索根目录不能是链接或越界路径",
            ));
        }
        if root.is_file() {
            if seen.insert(root.clone()) {
                result
                    .paths
                    .push((root.clone(), root.parent().unwrap_or(&root).to_owned()));
            }
            continue;
        }
        let mut stack: Vec<(PathBuf, Vec<std::sync::Arc<ignore::gitignore::Gitignore>>)> = vec![(
            root.clone(),
            inherited_rules(sb, &root, ignored, cancelled)?,
        )];
        while let Some((directory, mut rules)) = stack.pop() {
            check_cancel(cancelled)?;
            if started.elapsed() >= SCAN_TIME {
                result.truncated = true;
                break;
            }
            if !safe_entry(sb, &directory) {
                continue;
            }
            if !ignored && let Some(rule) = read_ignore(sb, &directory)? {
                rules.push(std::sync::Arc::new(rule));
            }
            let entries = fs::read_dir(&directory).map_err(|_| {
                invalid("mcp.couldn_t_read_the_search_directory", "无法读取检索目录")
            })?;
            for entry in entries {
                check_cancel(cancelled)?;
                scanned += 1;
                if scanned > SCAN_LIMIT || started.elapsed() >= SCAN_TIME {
                    result.truncated = true;
                    break;
                }
                let entry = entry.map_err(|_| {
                    invalid("mcp.couldn_t_read_a_directory_entry", "无法读取目录项")
                })?;
                let path = entry.path();
                // 包括被忽略的项也计入扫描限额，逐项检查取消，避免忽略大量文件时卡在迭代器里。
                if !safe_entry(sb, &path) {
                    continue;
                }
                let metadata = fs::symlink_metadata(&path).map_err(|_| {
                    invalid("mcp.couldn_t_read_file_properties", "无法读取文件属性")
                })?;
                if link(&metadata) {
                    continue;
                }
                let is_dir = metadata.is_dir();
                if !ignored && is_dir && entry.file_name() == ".git" {
                    continue;
                }
                let mut excluded = false;
                for rule in &rules {
                    let matched: ignore::Match<&ignore::gitignore::Glob> =
                        rule.matched(&path, is_dir);
                    if matched.is_ignore() {
                        excluded = true;
                    } else if matched.is_whitelist() {
                        excluded = false;
                    }
                }
                if !ignored && excluded {
                    continue;
                }
                if is_dir {
                    stack.push((path, rules.clone()));
                } else if metadata.is_file() && seen.insert(path.clone()) {
                    result.paths.push((path, root.clone()));
                }
            }
            if result.truncated {
                break;
            }
        }
        if result.truncated {
            break;
        }
    }
    result.paths.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}
fn inherited_rules(
    sb: &Sandbox,
    root: &Path,
    ignored: bool,
    cancelled: &AtomicBool,
) -> Result<Vec<std::sync::Arc<ignore::gitignore::Gitignore>>, AppError> {
    if ignored {
        return Ok(Vec::new());
    }
    let mut parents = Vec::new();
    for parent in root.ancestors().skip(1) {
        check_cancel(cancelled)?;
        if parent.to_str().is_none_or(|p| sb.resolve_read(p).is_err()) {
            break;
        }
        parents.push(parent);
    }
    let mut rules = Vec::new();
    for parent in parents.into_iter().rev() {
        check_cancel(cancelled)?;
        if let Some(rule) = read_ignore(sb, parent)? {
            rules.push(std::sync::Arc::new(rule));
        }
    }
    Ok(rules)
}
fn read_ignore(
    sb: &Sandbox,
    directory: &Path,
) -> Result<Option<ignore::gitignore::Gitignore>, AppError> {
    let path = directory.join(".gitignore");
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(invalid(
                "mcp.couldn_t_read_ignore_rule_properties",
                "无法读取忽略规则属性",
            ));
        }
        Ok(metadata) if link(&metadata) => {
            return Err(invalid(
                "mcp.ignore_rules_cannot_be_a_link",
                "忽略规则不能是链接",
            ));
        }
        Ok(_) => {}
    }
    let path = sb.resolve_read(path.to_str().ok_or_else(|| {
        invalid(
            "mcp.ignore_rule_path_is_not_valid_text",
            "忽略规则路径不是有效文字",
        )
    })?)?;
    let bytes = files::read_bounded(&path, 64 * 1024)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        invalid(
            "mcp.ignore_rules_are_not_valid_utf_8",
            "忽略规则不是有效的 UTF-8",
        )
    })?;
    let mut builder = ignore::gitignore::GitignoreBuilder::new(directory);
    for line in text.trim_start_matches('\u{feff}').lines() {
        builder
            .add_line(Some(path.clone()), line)
            .map_err(|_| invalid("mcp.invalid_ignore_rules", "忽略规则无效"))?;
    }
    builder
        .build()
        .map(Some)
        .map_err(|_| invalid("mcp.invalid_ignore_rules", "忽略规则无效"))
}
fn matches_glob(matcher: &GlobMatcher, path: &Path, root: &Path) -> bool {
    matcher.is_match(path.strip_prefix(root).unwrap_or(path))
        || path
            .file_name()
            .is_some_and(|name| matcher.is_match(Path::new(name)))
}
fn language(name: &str) -> Result<&'static [&'static str], AppError> {
    match name {
        "rust" => Ok(&["rs"]),
        "cs" | "csharp" => Ok(&["cs"]),
        "py" | "python" => Ok(&["py", "pyi"]),
        "js" => Ok(&["js", "jsx", "mjs", "cjs"]),
        "ts" => Ok(&["ts", "tsx", "mts", "cts"]),
        "c" => Ok(&["c", "h"]),
        "cpp" => Ok(&["cc", "cpp", "cxx", "h", "hpp", "hxx"]),
        "go" => Ok(&["go"]),
        "java" => Ok(&["java"]),
        "json" => Ok(&["json", "jsonc"]),
        "html" => Ok(&["html", "htm"]),
        "css" => Ok(&["css", "scss", "sass"]),
        "md" => Ok(&["md", "markdown"]),
        "yaml" => Ok(&["yaml", "yml"]),
        "toml" => Ok(&["toml"]),
        "xml" => Ok(&["xml", "csproj", "props"]),
        "sh" => Ok(&["sh", "bash", "zsh"]),
        "powershell" => Ok(&["ps1", "psm1", "psd1"]),
        "sql" => Ok(&["sql"]),
        "txt" => Ok(&["txt"]),
        _ => Err(invalid("mcp.unknown_file_type", "未知文件类型")),
    }
}
pub(super) fn search(
    sb: &Sandbox,
    args: &JsonObject,
    cancelled: &AtomicBool,
) -> Result<Value, AppError> {
    let started = Instant::now();
    let regex = regex::RegexBuilder::new(required(args, "pattern"))
        .case_insensitive(args.get("-i").and_then(Value::as_bool).unwrap_or(false))
        .multi_line(true)
        .size_limit(1024 * 1024)
        .build()
        .map_err(|_| {
            invalid(
                "mcp.regular_expression_is_invalid_or_too_large",
                "正则表达式无效或过大",
            )
        })?;
    let matcher = string(args, "glob").map(glob).transpose()?;
    let extensions = string(args, "type").map(language).transpose()?;
    let scan = collect(sb, string(args, "path"), false, cancelled, started)?;
    let mode = string(args, "output_mode").unwrap_or("files_with_matches");
    let mut output = Output::new(number(args, "offset", 0), number(args, "head_limit", 250));
    output.truncated = scan.truncated;
    let mut scanned_bytes = 0;
    'files: for (path, root) in scan.paths {
        check_cancel(cancelled)?;
        if started.elapsed() >= SCAN_TIME {
            output.truncated = true;
            break;
        }
        if matcher
            .as_ref()
            .is_some_and(|g| !matches_glob(g, &path, &root))
            || extensions.is_some_and(|e| {
                path.extension()
                    .and_then(|s| s.to_str())
                    .is_none_or(|s| !e.iter().any(|e| e.eq_ignore_ascii_case(s)))
            })
        {
            continue;
        }
        let path = sb.resolve_read(
            path.to_str()
                .ok_or_else(|| invalid("mcp.path_is_not_valid_text", "路径不是有效文字"))?,
        )?;
        let meta = fs::metadata(&path)
            .map_err(|_| invalid("mcp.couldn_t_read_file_properties", "无法读取文件属性"))?;
        if meta.len() > FILE_LIMIT as u64 {
            continue;
        }
        scanned_bytes += meta.len();
        if scanned_bytes > 256 * 1024 * 1024 {
            output.truncated = true;
            break;
        }
        let bytes = match files::read_bounded(&path, FILE_LIMIT) {
            Ok(b) => b,
            Err(e) if e.code == "mcp.file_too_large" => continue,
            Err(e) => return Err(e),
        };
        if binary(&bytes) {
            continue;
        }
        let doc = match Document::decode(&bytes) {
            Ok(d) => d,
            Err(e) if e.code == "mcp.encoding_unsupported" => continue,
            Err(e) => return Err(e),
        };
        let multiline = args
            .get("multiline")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // 常用的文件名和计数模式无需保存每一行，短行很多的文件也不扩张成大型索引。
        if mode == "files_with_matches" || mode == "count" && !multiline {
            let mut count = 0;
            if multiline {
                count = usize::from(regex.is_match(&doc.text));
            } else {
                for line in doc.text.lines() {
                    check_cancel(cancelled)?;
                    if started.elapsed() >= SCAN_TIME {
                        output.truncated = true;
                        break 'files;
                    }
                    if regex.is_match(line) {
                        count += 1;
                        if mode == "files_with_matches" {
                            break;
                        }
                    }
                }
            }
            if count > 0 {
                let value = if mode == "files_with_matches" {
                    json!(path)
                } else {
                    json!({"path":path,"count":count})
                };
                if !output.push(value) {
                    break;
                }
            }
            continue;
        }
        let lines: Vec<_> = doc.text.lines().collect();
        let mut matched = BTreeSet::new();
        if args
            .get("multiline")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let starts: Vec<_> = std::iter::once(0)
                .chain(doc.text.match_indices('\n').map(|(i, _)| i + 1))
                .collect();
            for found in regex.find_iter(&doc.text) {
                check_cancel(cancelled)?;
                if started.elapsed() >= SCAN_TIME {
                    output.truncated = true;
                    break 'files;
                }
                let first = starts
                    .partition_point(|s| *s <= found.start())
                    .saturating_sub(1);
                let last = starts
                    .partition_point(|s| *s <= found.end().saturating_sub(1).max(found.start()))
                    .saturating_sub(1);
                matched.extend(first..=last.min(lines.len().saturating_sub(1)));
            }
        } else {
            for (index, line) in lines.iter().enumerate() {
                check_cancel(cancelled)?;
                if started.elapsed() >= SCAN_TIME {
                    output.truncated = true;
                    break 'files;
                }
                if regex.is_match(line) {
                    matched.insert(index);
                }
            }
        }
        if matched.is_empty() {
            continue;
        }
        match mode {
            "files_with_matches" => {
                if !output.push(json!(path)) {
                    break;
                }
            }
            "count" => {
                if !output.push(json!({"path":path,"count":matched.len()})) {
                    break;
                }
            }
            "content" => {
                let context = number(args, "-C", 0) as usize;
                let before = number(args, "-B", context as u64) as usize;
                let after = number(args, "-A", context as u64) as usize;
                let mut selected = BTreeSet::new();
                for index in &matched {
                    selected.extend(
                        index.saturating_sub(before)
                            ..=(index + after).min(lines.len().saturating_sub(1)),
                    );
                }
                for index in selected {
                    check_cancel(cancelled)?;
                    let mut value = json!({"path":path,"text":lines.get(index).unwrap_or(&""),"match":matched.contains(&index),"encoding":doc.name(),"line_ending":doc.line_ending()});
                    if args.get("-n").and_then(Value::as_bool).unwrap_or(true) {
                        value["line_number"] = json!(index + 1);
                    }
                    if !output.push(value) {
                        break;
                    }
                }
                if output.full {
                    break;
                }
            }
            _ => return Err(invalid("mcp.unknown_output_mode", "未知输出模式")),
        }
    }
    Ok(json!({"output_mode":mode,"results":output.values,"truncated":output.truncated}))
}
struct Output {
    values: Vec<Value>,
    offset: u64,
    limit: u64,
    bytes: usize,
    truncated: bool,
    full: bool,
}
impl Output {
    fn new(offset: u64, limit: u64) -> Self {
        Self {
            values: Vec::new(),
            offset,
            limit,
            bytes: 128,
            truncated: false,
            full: false,
        }
    }
    fn push(&mut self, value: Value) -> bool {
        if self.offset > 0 {
            self.offset -= 1;
            return true;
        }
        let bytes = value.to_string().len() + 1;
        if self.values.len() as u64 >= self.limit || self.bytes + bytes > OUTPUT_LIMIT {
            self.truncated = true;
            self.full = true;
            return false;
        }
        self.bytes += bytes;
        self.values.push(value);
        true
    }
}
pub(super) fn find_files(
    sb: &Sandbox,
    args: &JsonObject,
    cancelled: &AtomicBool,
) -> Result<Value, AppError> {
    let matcher = glob(required(args, "pattern"))?;
    let mut scan = collect(
        sb,
        string(args, "path"),
        args.get("include_ignored")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        cancelled,
        Instant::now(),
    )?;
    scan.paths.retain(|(p, r)| matches_glob(&matcher, p, r));
    let mut paths: Vec<_> = scan
        .paths
        .into_iter()
        .map(|(p, _)| {
            let time = fs::symlink_metadata(&p)
                .map(|m| files::mtime(&m))
                .unwrap_or(0);
            (p, time)
        })
        .collect();
    paths.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut output = Output::new(0, number(args, "head_limit", 250));
    output.truncated = scan.truncated;
    for (path, _) in paths {
        check_cancel(cancelled)?;
        if !output.push(json!(path)) {
            break;
        }
    }
    Ok(json!({"paths":output.values,"truncated":output.truncated}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(value: Value) -> JsonObject {
        value.as_object().unwrap().clone()
    }
    fn fixture() -> tempfile::TempDir {
        let root = crate::test_support::tempdir();
        fs::write(root.path().join(".gitignore"), "ignored/\n*.skip\n").unwrap();
        fs::create_dir(root.path().join("ignored")).unwrap();
        fs::write(root.path().join("ignored/a.rs"), "needle").unwrap();
        fs::write(
            root.path().join("a.rs"),
            "before\nNeedle one\nafter\nneedle two\nlast\n",
        )
        .unwrap();
        fs::write(root.path().join("b.py"), "needle\nnext\n").unwrap();
        fs::write(root.path().join("x.skip"), "needle").unwrap();
        fs::write(root.path().join("binary"), b"needle\0").unwrap();
        let huge = fs::File::create(root.path().join("large")).unwrap();
        huge.set_len(FILE_LIMIT as u64 + 1).unwrap();
        root
    }
    #[test]
    fn 搜索过滤上下文模式分页与多行() {
        let root = fixture();
        let sb = Sandbox::new(&[root.path().into()]);
        let cancel = AtomicBool::new(false);
        let result = search(&sb, &args(json!({"pattern":"needle"})), &cancel).unwrap();
        assert_eq!(result["results"].as_array().unwrap().len(), 2);
        let result=search(&sb,&args(json!({"pattern":"needle","-i":true,"type":"rust","glob":"**/*.rs","output_mode":"count"})),&cancel).unwrap();
        assert_eq!(result["results"][0]["count"], 2);
        let result=search(&sb,&args(json!({"pattern":"Needle","path":root.path().join("a.rs"),"output_mode":"content","-A":1,"-B":1})),&cancel).unwrap();
        assert_eq!(result["results"].as_array().unwrap().len(), 3);
        assert_eq!(result["results"][0]["line_number"], 1);
        let result=search(&sb,&args(json!({"pattern":"Needle","type":"rust","output_mode":"content","-C":1,"-n":false,"offset":1,"head_limit":1})),&cancel).unwrap();
        assert_eq!(result["results"][0]["text"], "Needle one");
        assert!(result["results"][0].get("line_number").is_none());
        assert_eq!(result["truncated"], true);
        let result = search(
            &sb,
            &args(json!({"pattern":"needle\\nnext","multiline":true,"output_mode":"content"})),
            &cancel,
        )
        .unwrap();
        assert_eq!(result["results"].as_array().unwrap().len(), 2);
        assert!(search(&sb, &args(json!({"pattern":"["})), &cancel).is_err());
        assert!(
            search(
                &sb,
                &args(json!({"pattern":"x","type":"nonesuch"})),
                &cancel
            )
            .is_err()
        );
    }
    #[test]
    fn 搜索编码输出字节上限与取消() {
        let root = fixture();
        let sb = Sandbox::new(&[root.path().into()]);
        fs::write(
            root.path().join("wide.txt"),
            [
                vec![0xff, 0xfe],
                "中文\r\n"
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ]
            .concat(),
        )
        .unwrap();
        let result = search(
            &sb,
            &args(json!({"pattern":"中文","output_mode":"content"})),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result["results"][0]["text"], "中文");
        fs::write(root.path().join("big.txt"), "needle ".repeat(50000)).unwrap();
        let result = search(
            &sb,
            &args(json!({"pattern":"needle","output_mode":"content"})),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(result.to_string().len() <= OUTPUT_LIMIT);
        assert_eq!(result["truncated"], true);
        assert_eq!(
            search(&sb, &args(json!({"pattern":"x"})), &AtomicBool::new(true))
                .unwrap_err()
                .code,
            "mcp.cancelled"
        );
    }
    #[test]
    fn 查找按修改时间排序忽略规则与上限() {
        let root = fixture();
        let sb = Sandbox::new(&[root.path().into()]);
        let cancel = AtomicBool::new(false);
        // 改修改时间要有写权限（Windows 上只读打开会被拒绝）
        fs::File::options()
            .write(true)
            .open(root.path().join("b.py"))
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        let result = find_files(
            &sb,
            &args(json!({"pattern":"**/*","head_limit":1})),
            &cancel,
        )
        .unwrap();
        assert_eq!(result["truncated"], true);
        assert!(!result["paths"][0].as_str().unwrap().ends_with("b.py"));
        let result = find_files(&sb, &args(json!({"pattern":"**/*.rs"})), &cancel).unwrap();
        assert_eq!(result["paths"].as_array().unwrap().len(), 1);
        let result = find_files(
            &sb,
            &args(json!({"pattern":"**/*.rs","include_ignored":true})),
            &cancel,
        )
        .unwrap();
        assert_eq!(result["paths"].as_array().unwrap().len(), 2);
        assert_eq!(
            find_files(&sb, &args(json!({"pattern":"*"})), &AtomicBool::new(true))
                .unwrap_err()
                .code,
            "mcp.cancelled"
        );
        assert!(find_files(&sb, &args(json!({"pattern":"["})), &cancel).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn 遍历不跟随符号链接或越界忽略规则() {
        let root = fixture();
        let outside = crate::test_support::tempdir();
        fs::write(outside.path().join("secret.rs"), "needle").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        let sb = Sandbox::new(&[root.path().into()]);
        let result = find_files(
            &sb,
            &args(json!({"pattern":"**/*.rs","include_ignored":true})),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result["paths"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn 指定子目录仍遵守父级规则且嵌套白名单覆盖() {
        let root = crate::test_support::tempdir();
        fs::create_dir(root.path().join("sub")).unwrap();
        fs::write(root.path().join(".gitignore"), "*.txt\n").unwrap();
        fs::write(root.path().join("sub/.gitignore"), "!keep.txt\n").unwrap();
        fs::write(root.path().join("sub/keep.txt"), "match").unwrap();
        fs::write(root.path().join("sub/drop.txt"), "match").unwrap();
        let sb = Sandbox::new(&[root.path().into()]);
        let result = find_files(
            &sb,
            &args(json!({"pattern":"**/*.txt","path":root.path().join("sub")})),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(result["paths"].as_array().unwrap().len(), 1);
        assert!(result["paths"][0].as_str().unwrap().ends_with("keep.txt"));
    }
    #[cfg(unix)]
    #[test]
    fn 根目录的忽略规则也不能经链接读取范围外文件() {
        let root = crate::test_support::tempdir();
        let outside = crate::test_support::tempdir();
        fs::write(outside.path().join("ignore"), "*.rs").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("ignore"),
            root.path().join(".gitignore"),
        )
        .unwrap();
        let sb = Sandbox::new(&[root.path().into()]);
        assert!(find_files(&sb, &args(json!({"pattern":"*"})), &AtomicBool::new(false)).is_err());
    }
}
