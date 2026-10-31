//! 检查固定错误说明及译文覆盖；只读源码，不创建临时文件。
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
struct Token {
    text: String,
    literal: bool,
    line: usize,
}

fn chinese(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{3400}'..='\u{9fff}'))
}

// 字符串保持为一个词元，避免把其中的括号、注释标记或伪代码当成源码。
fn lex(source: &str) -> Vec<Token> {
    let pattern = regex::Regex::new(
        r###"(?s)//[^\n]*|/\*|(?:br|r)\#*"|b?"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])'|[\p{L}_][\p{L}\p{N}_]*|[^\s]"###,
    )
    .unwrap();
    let mut line = 1;
    let mut previous = 0;
    let mut out = Vec::new();
    while let Some(matched) = pattern.find_at(source, previous) {
        line += source[previous..matched.start()].matches('\n').count();
        let raw = matched.as_str();
        let token_line = line;
        let mut end = matched.end();
        let raw_string = (raw.starts_with("r") || raw.starts_with("br")) && raw.ends_with('"');
        if raw_string {
            let hashes = raw.bytes().filter(|&c| c == b'#').count();
            let delimiter = format!("\"{}", "#".repeat(hashes));
            end += source[end..].find(&delimiter).unwrap() + delimiter.len();
        } else if raw == "/*" {
            let mut depth = 1;
            while end < source.len() && depth > 0 {
                if source[end..].starts_with("/*") {
                    depth += 1;
                    end += 2;
                } else if source[end..].starts_with("*/") {
                    depth -= 1;
                    end += 2;
                } else {
                    end += source[end..].chars().next().unwrap().len_utf8();
                }
            }
        }
        let raw = &source[matched.start()..end];
        line += raw.matches('\n').count();
        previous = end;
        if raw.starts_with("//") || raw.starts_with("/*") {
            continue;
        }
        let literal = raw_string || raw.starts_with('"') || raw.starts_with("b\"");
        let text = if literal {
            let start = raw.find('"').unwrap() + 1;
            let end = raw.rfind('"').unwrap();
            raw[start..end].to_owned()
        } else {
            raw.to_owned()
        };
        out.push(Token {
            text,
            literal,
            line: token_line,
        });
    }
    out
}

fn symbol(tokens: &[Token], index: usize, text: &str) -> bool {
    tokens
        .get(index)
        .is_some_and(|token| !token.literal && token.text == text)
}

fn closing(tokens: &[Token], start: usize) -> usize {
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(start) {
        if token.literal {
            continue;
        }
        match token.text.as_str() {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    tokens.len().saturating_sub(1)
}

fn production(source: &str) -> Vec<Token> {
    let tokens = lex(source);
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if symbol(&tokens, i, "#") {
            let inner = symbol(&tokens, i + 1, "!");
            let bracket = i + 1 + usize::from(inner);
            if symbol(&tokens, bracket, "[") {
                let end = closing(&tokens, bracket);
                let attribute = tokens[bracket + 1..end]
                    .iter()
                    .map(|token| token.text.as_str())
                    .collect::<String>();
                if (attribute == "test"
                    || attribute.starts_with("cfg(") && attribute.contains("test"))
                    && !attribute.contains("not(test)")
                {
                    if inner {
                        return Vec::new();
                    }
                    i = end + 1;
                    // 同时支持测试模块、函数、外部模块声明及结构体里的测试字段。
                    while i < tokens.len()
                        && !["{", ";", ","]
                            .iter()
                            .any(|value| symbol(&tokens, i, value))
                    {
                        i += 1;
                    }
                    if symbol(&tokens, i, "{") {
                        i = closing(&tokens, i);
                    }
                    i += 1;
                    continue;
                }
            }
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

fn arguments(tokens: &[Token], open: usize) -> Vec<(usize, usize)> {
    let end = closing(tokens, open);
    let mut out = Vec::new();
    let mut start = open + 1;
    let mut i = start;
    while i < end {
        if ["(", "[", "{"].iter().any(|value| symbol(tokens, i, value)) {
            i = closing(tokens, i) + 1;
            continue;
        }
        if symbol(tokens, i, ",") {
            if i > start {
                out.push((start, i));
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < end {
        out.push((start, end));
    }
    out
}

fn error_functions(tokens: &[Token]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for i in 0..tokens.len().saturating_sub(3) {
        if !symbol(tokens, i, "fn") || !symbol(tokens, i + 2, "(") {
            continue;
        }
        let end = closing(tokens, i + 2);
        if symbol(tokens, end + 1, "-")
            && symbol(tokens, end + 2, ">")
            && symbol(tokens, end + 3, "AppError")
        {
            names.insert(tokens[i + 1].text.clone());
        }
    }
    names
}

// 识别 AppError 构造、返回 AppError 的辅助函数和链式调用；不按 with 的名字猜类型。
fn error_receiver(
    tokens: &[Token],
    end: usize,
    functions: &BTreeSet<String>,
    variables: &BTreeSet<String>,
) -> bool {
    if !symbol(tokens, end, ")") {
        return variables.contains(&tokens[end].text);
    }
    let Some(open) = (0..end)
        .rev()
        .find(|&i| symbol(tokens, i, "(") && closing(tokens, i) == end)
    else {
        return false;
    };
    if open == 0 {
        return false;
    }
    let name = &tokens[open - 1].text;
    if open >= 4 && symbol(tokens, open - 4, "AppError") {
        return true;
    }
    if open >= 3 && symbol(tokens, open - 2, ".") {
        return error_receiver(tokens, open - 3, functions, variables);
    }
    functions.contains(name)
}

fn error_variables(tokens: &[Token], functions: &BTreeSet<String>) -> BTreeSet<String> {
    let mut variables = BTreeSet::new();
    for i in 0..tokens.len().saturating_sub(2) {
        if symbol(tokens, i + 1, ":") && symbol(tokens, i + 2, "AppError") {
            variables.insert(tokens[i].text.clone());
        }
    }
    for i in 0..tokens.len().saturating_sub(3) {
        if !symbol(tokens, i, "let") {
            continue;
        }
        let name = i + 1 + usize::from(symbol(tokens, i + 1, "mut"));
        if !symbol(tokens, name + 1, "=") {
            continue;
        }
        let end = (name + 2..tokens.len())
            .find(|&j| symbol(tokens, j, ";"))
            .unwrap_or(tokens.len());
        if (name + 2..end).any(|j| {
            symbol(tokens, j, "AppError")
                || functions.contains(&tokens[j].text) && symbol(tokens, j + 1, "(")
        }) {
            variables.insert(tokens[name].text.clone());
        }
    }
    variables
}

fn untranslated(tokens: &[Token], functions: &BTreeSet<String>) -> Vec<usize> {
    let variables = error_variables(tokens, functions);
    let mut lines = Vec::new();
    for i in 2..tokens.len().saturating_sub(1) {
        if !symbol(tokens, i, "with")
            || !symbol(tokens, i - 1, ".")
            || !symbol(tokens, i + 1, "(")
            || !error_receiver(tokens, i - 2, functions, &variables)
        {
            continue;
        }
        let args = arguments(tokens, i + 1);
        if args.len() == 2
            && tokens[args[1].0..args[1].1]
                .iter()
                .any(|token| token.literal && chinese(&token.text))
        {
            lines.push(tokens[i].line);
        }
    }
    lines
}

fn used_keys(tokens: &[Token]) -> BTreeSet<String> {
    let key_pattern = regex::Regex::new(r"^[a-z0-9_]+\.[a-z0-9_]+$").unwrap();
    let mut keys = BTreeSet::new();
    // 辅助函数和元组的键按核心模块命名；cmd.exe 等带中文标签的程序名不算键。
    let modules: BTreeSet<_> = include_str!("lib.rs")
        .lines()
        .filter_map(|line| line.strip_prefix("pub mod ")?.strip_suffix(';'))
        .collect();
    for i in 0..tokens.len().saturating_sub(1) {
        let method = &tokens[i].text;
        if !tokens[i].literal
            && ["with_reason", "with_text", "failure"].contains(&method.as_str())
            && symbol(tokens, i + 1, "(")
        {
            let args = arguments(tokens, i + 1);
            let key_index = usize::from(method != "with_reason");
            if let Some(&(start, end)) = args.get(key_index)
                && end == start + 1
                && tokens[start].literal
            {
                // failure 的旧平铺键只在内置 SSH 中使用；其他 failure 的第二项为新键。
                let key = &tokens[start].text;
                if key_pattern.is_match(key)
                    || method == "failure"
                        && args.len() == 3
                        && tokens[args[2].0..args[2].1]
                            .iter()
                            .any(|t| t.literal && chinese(&t.text))
                        && !chinese(key)
                        && !key.contains(' ')
                {
                    keys.insert(key.clone());
                }
            }
        }
        // 辅助函数、分支元组和闭包都把键放在中文原文前；格式化原文也在这里检查。
        if tokens[i].literal
            && key_pattern.is_match(&tokens[i].text)
            && tokens[i]
                .text
                .split_once('.')
                .is_some_and(|(module, _)| modules.contains(module))
            && symbol(tokens, i + 1, ",")
        {
            let next = i + 2;
            if tokens
                .get(next)
                .is_some_and(|token| token.literal && chinese(&token.text))
                || symbol(tokens, next, "format")
                || symbol(tokens, next, "&") && symbol(tokens, next + 1, "format")
            {
                keys.insert(tokens[i].text.clone());
            }
        }
    }
    keys
}

fn shell_keys(source: &str) -> BTreeSet<String> {
    // 脚本错误码使用 vnc.xxx；其余 vnc.xxx 字面量均是固定说明键。
    let source = regex::Regex::new(r#""error":"vnc\.[a-z0-9_]+""#)
        .unwrap()
        .replace_all(source, "");
    regex::Regex::new(r"\bvnc\.[a-z0-9_]+\b")
        .unwrap()
        .find_iter(&source)
        .map(|m| m.as_str().to_owned())
        .collect()
}

fn shell_untranslated(source: &str) -> Vec<usize> {
    let call = regex::Regex::new(r#"\bfail\s+[a-z_]+\s+['"]([^'"]*)['"]"#).unwrap();
    source
        .lines()
        .enumerate()
        .filter_map(|(line, text)| {
            let reason = call.captures(text)?.get(1)?;
            chinese(reason.as_str()).then_some(line + 1)
        })
        .collect()
}

fn sources(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if !["tests", "test_support"]
                .iter()
                .any(|name| path.ends_with(name))
            {
                paths.extend(sources(&path));
            }
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("tests.rs")
        {
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

fn flatten(value: &serde_json::Value, prefix: &str, out: &mut BTreeMap<String, String>) {
    if let Some(object) = value.as_object() {
        for (name, value) in object {
            let key = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            flatten(value, &key, out);
        }
    } else {
        out.insert(
            prefix.to_owned(),
            value.as_str().unwrap_or_default().to_owned(),
        );
    }
}

#[test]
fn 固定错误参数不能绕过翻译且所有译文键双向对应() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // 核心库和前端外壳（src-tauri）都按同样的规则检查：外壳也会把固定中文放进错误参数
    let core: Vec<_> = sources(&root)
        .into_iter()
        .chain(sources(&root.join("../../../src-tauri/src")))
        .map(|path| {
            let tokens = production(&fs::read_to_string(&path).unwrap());
            (path, tokens)
        })
        .collect();
    let functions = core
        .iter()
        .flat_map(|(_, tokens)| error_functions(tokens))
        .collect();
    let mut errors = Vec::new();
    let mut used = BTreeSet::new();
    for (path, tokens) in &core {
        for line in untranslated(tokens, &functions) {
            errors.push(format!(
                "{}:{line}: 固定中文必须使用 with_text 或 with_reason",
                path.display()
            ));
        }
        used.extend(used_keys(tokens));
    }
    for entry in fs::read_dir(root.join("vnc")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "sh") {
            let source = fs::read_to_string(&path).unwrap();
            for line in shell_untranslated(&source) {
                errors.push(format!("{}:{line}: fail 的中文原因缺少键", path.display()));
            }
            used.extend(shell_keys(&source));
        }
    }
    // 代理拒绝原因的键写在引擎的 RefuseReason::key() 里，core 经 with_reason(reason.key(), …) 使用
    let engine_proxy = fs::read_to_string(root.join("../../ssh-engine/src/proxy.rs")).unwrap();
    let proxy_key = regex::Regex::new(r#""(proxy\.[a-z0-9_]+)""#).unwrap();
    used.extend(
        proxy_key
            .captures_iter(&engine_proxy)
            .map(|m| m[1].to_owned()),
    );
    let mut translations = Vec::new();
    for (language, source) in [
        ("zh-CN", include_str!("../../../src/locales/zh-CN.json")),
        ("en", include_str!("../../../src/locales/en.json")),
    ] {
        let locale: serde_json::Value = serde_json::from_str(source).unwrap();
        let mut details = BTreeMap::new();
        flatten(&locale["errors"]["detail"], "", &mut details);
        for key in &used {
            if !details.get(key).is_some_and(|text| !text.trim().is_empty()) {
                errors.push(format!("{language}: 缺少说明译文 {key}"));
            }
        }
        for key in details.keys() {
            if !used.contains(key) {
                errors.push(format!("{language}: 未使用的说明译文 {key}"));
            }
        }
        translations.push(details);
    }
    let placeholder = regex::Regex::new(r"\{\{([a-z_0-9]+)\}\}").unwrap();
    for (key, chinese) in &translations[0] {
        let variables = |text: &str| {
            placeholder
                .captures_iter(text)
                .map(|m| m[1].to_owned())
                .collect::<BTreeSet<_>>()
        };
        if let Some(english) = translations[1].get(key) {
            assert_eq!(
                variables(chinese),
                variables(english),
                "占位符不一致: {key}"
            );
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn 扫描识别链式辅助函数格式化和变量且不误报其他类型() {
    let tokens = production(
        r###"
fn failed() -> AppError { AppError::new("demo.failed") }
fn inspect(error: AppError) {
    failed().with("reason", format!("失败（{}）", 3));
    let saved = AppError::new("demo.failed");
    saved.with("field", "命令");
    error.with("reason", "原因");
    Other::new().with("reason", "其他对象");
    let other = Other::new();
    other.with("reason", "其他对象");
    // AppError::new("demo.failed").with("reason", "注释");
    let text = r##"AppError::new("demo.failed").with("reason", "字符串")"##;
}
"###,
    );
    assert_eq!(
        untranslated(&tokens, &error_functions(&tokens)),
        vec![4, 6, 7]
    );
}

#[test]
fn 测试模块测试字段和字符串不会遮住后续业务代码() {
    let tokens = production(
        r#"
#[cfg(test)]
mod tests { fn sample() { AppError::new("demo").with("reason", "测试"); } }
struct Data { #[cfg(test)] count: usize, value: bool }
fn real() { AppError::new("demo").with("reason", "业务"); }
#[test]
fn another() { AppError::new("demo").with("reason", "测试"); }
"#,
    );
    assert_eq!(untranslated(&tokens, &BTreeSet::new()), vec![5]);
}

#[test]
fn 键收集覆盖直接方法辅助函数元组及脚本默认值() {
    let keys = used_keys(&production(
        r#"
error.with_reason("mcp.direct", "中文");
error.with_text("operation", "mcp.operation", "操作");
helper("mcp.helper", format!("原因 {}", kind));
let pair = ("mcp.branch", "分支");
failure("ssh.failed", "network", "网络");
"#,
    ));
    assert_eq!(
        keys,
        [
            "mcp.direct",
            "mcp.operation",
            "mcp.helper",
            "mcp.branch",
            "network"
        ]
        .map(str::to_owned)
        .into()
    );
    assert_eq!(
        shell_untranslated("fail failed '中文'\nfail failed 'vnc.reason' '中文'"),
        vec![1]
    );
    assert_eq!(
        shell_keys(
            r#"json "${2:-vnc.default}"; reason_key="vnc.apt"; printf '{"error":"vnc.failed"}'"#
        ),
        ["vnc.default", "vnc.apt"].map(str::to_owned).into()
    );
}
