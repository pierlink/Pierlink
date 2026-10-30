//! 手写工具 schema；校验只覆盖这里实际使用的类型与约束。
use rmcp::{
    ErrorData,
    model::{JsonObject, Tool},
};
use serde_json::{Value, json};
use std::sync::LazyLock;

pub(super) static TOOLS: LazyLock<Vec<Tool>> = LazyLock::new(|| {
    let string = json!({"type":"string"});
    let target =
        json!({"type":"string","minLength":1,"description":"list_targets 返回的 target 名或 id"});
    let id = json!({"type":"string","minLength":1});
    let timeout = json!({"type":"integer","minimum":1,"maximum":1800,"default":120});
    let boolean = json!({"type":"boolean"});
    let number = json!({"type":"integer","minimum":0});
    let mut keys: Vec<String> = [
        "Enter",
        "Tab",
        "Esc",
        "Backspace",
        "Delete",
        "Up",
        "Down",
        "Left",
        "Right",
        "Home",
        "End",
        "PageUp",
        "PageDown",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    keys.extend(('A'..='Z').map(|key| format!("Ctrl+{key}")));
    let wait_timeout = json!({"type":"integer","minimum":1,"maximum":1800,"default":60});
    let sync_timeout = json!({"type":"integer","minimum":1,"maximum":1800,"default":300});
    [
        ("list_targets", "列出入口允许的目标及在线状态；folders 为可用授权目录，unavailable_folders 列出不可用目录的错误码和原因。", json!({}), json!([])),
        ("run_command", "在目标执行非交互命令，检查 exit_code；返回 stdout、stderr、cwd、shell、truncated、timed_out、duration_ms。Windows 默认 powershell 和用户目录，shell 只能选 list_targets 列出的项。background:true 立即返回 job_id（不受 timeout_secs 限制），用 job_output 读取输出，用 job_stop 结束进程树。每入口最多 8 个后台任务；后台任务目前只支持 Windows，服务器长任务请用 AI 终端。前台输出各保留 256 KiB 头尾，最长 1800 秒。", json!({"target":target,"command":{"type":"string","minLength":1},"cwd":string,"timeout_secs":timeout,"shell":{"type":"string","enum":["powershell","pwsh","cmd","git-bash"]},"background":boolean}), json!(["target","command"])),
        ("open_terminal", "请用户界面打开可见的 AI 终端，返回 terminal_id。cwd 会以一行 cd 的形式回显。", json!({"target":target,"cwd":string,"title":string}), json!(["target"])),
        ("list_terminals", "返回 ai_terminals（可操作终端，source.kind 为 opened 或 handed_over，含 cwd，未知为 null）和 user_terminals（用户终端元数据，handed_over 表示是否交给当前入口）。仅列获准目标，不含地址；未交出的终端不提供目录和内容。", json!({}), json!([])),
        ("close_terminal", "关闭已登记的 AI 终端，不能关闭用户自己的终端。", json!({"terminal_id":id}), json!(["terminal_id"])),
        ("terminal_send", "发送文字或按键。成功只表示已入队，要用 terminal_wait 确认；屏幕在等密码时发送文字会被拒绝，请用户在 Pierlink 里输入，按键仍允许。", json!({"terminal_id":id,"text":string,"keys":{"type":"array","items":{"type":"string","enum":keys}}}), json!(["terminal_id"])),
        ("terminal_read", "从 since 游标读取新输出，最多 64 KiB；返回 text、cursor、lost。", json!({"terminal_id":id,"since":number}), json!(["terminal_id"])),
        ("terminal_screen", "读取终端当前屏幕的文字。", json!({"terminal_id":id}), json!(["terminal_id"])),
        ("terminal_wait", "等 pattern 正则匹配或输出安静 quiet_ms；返回 matched、text、cursor，最长 1800 秒。", json!({"terminal_id":id,"pattern":string,"quiet_ms":{"type":"integer","minimum":1,"maximum":1800000},"since":number,"timeout_secs":wait_timeout}), json!(["terminal_id"])),
        ("read_file", "读取 Windows 上授权文件夹内的文件，file_path 用 Windows 绝对路径。默认按行读，offset 从 1 开始，limit 默认 2000、最多 5000；返回带行号的文字、编码、换行、修改时间和 sha256；超过 64 MiB 时 sha256 为 null，sha256_skipped_reason 说明原因。byte_offset/max_bytes 改为按字节读取。", json!({"file_path":id,"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":5000,"default":2000},"byte_offset":number,"max_bytes":{"type":"integer","minimum":1,"maximum":1048576}}), json!(["file_path"])),
        ("write_file", "写入 Windows 文件，file_path 用 Windows 绝对路径。内容最多 5 MiB；不能覆盖已有二进制文件或超过 5 MiB 的已有文件。保持已有编码、BOM 和换行。expected_sha256 不符时拒绝覆盖；脚本和启动文件需要确认。", json!({"file_path":id,"content":string,"expected_sha256":id}), json!(["file_path","content"])),
        ("edit_file", "精确替换 Windows 文件中的文字，file_path 用 Windows 绝对路径。old_string 必须唯一，replace_all 可替换所有匹配；保持原编码、BOM 和换行。可用 expected_sha256 检查读后变化。", json!({"file_path":id,"old_string":id,"new_string":string,"replace_all":boolean,"expected_sha256":id}), json!(["file_path","old_string","new_string"])),
        ("search", "在 Windows 授权文件夹中用 Rust regex 搜索，path 用 Windows 绝对路径，省略时搜索全部授权文件夹。遵守 .gitignore，不跟随链接；跳过二进制及超过 10 MiB 的文件。结果按路径排序，最多扫描 20000 项、256 MiB、10 秒，输出最多 256 KiB；达到限制返回 truncated。", json!({"pattern":string,"path":id,"glob":id,"type":id,"output_mode":{"type":"string","enum":["content","files_with_matches","count"],"default":"files_with_matches"},"-i":boolean,"-n":boolean,"multiline":boolean,"-A":{"type":"integer","minimum":0,"maximum":5000},"-B":{"type":"integer","minimum":0,"maximum":5000},"-C":{"type":"integer","minimum":0,"maximum":5000},"head_limit":{"type":"integer","minimum":1,"maximum":10000,"default":250},"offset":number}), json!(["pattern"])),
        ("find_files", "在 Windows 授权文件夹中按 glob 找文件，path 用 Windows 绝对路径，省略时搜索全部授权文件夹。按修改时间从新到旧返回路径；遵守 .gitignore，include_ignored 可关闭忽略规则；不跟随链接，达到扫描或输出限制返回 truncated。", json!({"pattern":id,"path":id,"head_limit":{"type":"integer","minimum":1,"maximum":10000,"default":250},"include_ignored":boolean}), json!(["pattern"])),
        ("job_output", "读取当前入口后台任务的新输出，since 是字节游标；返回 stdout、stderr、cursor、lost、running 和 exit_code。输出仅保留最近 1 MiB。", json!({"job_id":id,"since":number}), json!(["job_id"])),
        ("job_stop", "结束当前入口后台任务的整棵进程树。", json!({"job_id":id}), json!(["job_id"])),
        ("list_jobs", "列出当前入口的后台任务；完成的任务保留 30 分钟。", json!({}), json!([])),
        ("list_dir", "列出 Windows 授权文件夹内容，path 使用 Windows 绝对路径；每项含 size、mtime_ms、is_dir，最多 2000 项。", json!({"path":id}), json!(["path"])),
        ("sync_status", "查询允许访问的同步任务状态、待同步字节和各服务器完成度；task 为任务名或 ID。", json!({"task":id}), json!([])),
        ("sync_wait", "先扫描本机改动再等待允许访问的目标同步完成，改完文件后先等待再编译。", json!({"task":id,"timeout_secs":sync_timeout}), json!([])),
    ].into_iter().map(|(name, description, properties, required)| {
        let schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        let mut tool = Tool::new(name, description, schema.as_object().cloned().unwrap_or_default());
        if matches!(name,"list_targets"|"read_file"|"edit_file"|"write_file"|"search"|"find_files"|"run_command"|"job_output") { tool.meta = Some(serde_json::from_value(json!({"anthropic/alwaysLoad":true})).unwrap_or_default()); }
        tool
    }).collect()
});

pub(super) fn validate(name: &str, args: &JsonObject) -> Result<(), ErrorData> {
    let tool = TOOLS
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| invalid("mcp.unknown_tool", "未知工具"))?;
    let properties = &tool.input_schema["properties"];
    if let Some(required) = tool.input_schema["required"].as_array() {
        for field in required.iter().filter_map(Value::as_str) {
            if !args.contains_key(field) {
                return Err(invalid("mcp.required_parameter_is_missing", "缺少必需参数"));
            }
        }
    }
    for (key, value) in args {
        let Some(schema) = properties.get(key) else {
            return Err(invalid("mcp.unknown_parameter", "包含未知参数"));
        };
        if !valid_value(value, schema) {
            return Err(invalid(
                "mcp.invalid_parameter_type_or_value",
                "参数类型或取值不合法",
            ));
        }
    }
    if name == "terminal_send"
        && !args.contains_key("text")
        && args
            .get("keys")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        return Err(invalid("mcp.text_or_keys_is_required", "需要 text 或 keys"));
    }
    if name == "terminal_wait"
        && let Some(pattern) = args.get("pattern").and_then(Value::as_str)
        && regex::Regex::new(pattern).is_err()
    {
        return Err(invalid(
            "mcp.pattern_is_not_a_valid_regular_expression",
            "pattern 不是有效的正则表达式",
        ));
    }
    if name == "read_file"
        && (args.contains_key("offset") || args.contains_key("limit"))
        && (args.contains_key("byte_offset") || args.contains_key("max_bytes"))
    {
        return Err(invalid(
            "mcp.line_and_byte_parameters_cannot_be_combined",
            "按行和按字节参数不能混用",
        ));
    }
    if name == "write_file"
        && args
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|text| text.len() > 5 * 1024 * 1024)
    {
        return Err(invalid(
            "mcp.file_content_exceeds_5_mib",
            "文件内容超过 5 MiB",
        ));
    }
    Ok(())
}
fn valid_value(value: &Value, schema: &Value) -> bool {
    match schema["type"].as_str() {
        Some("string") => value.as_str().is_some_and(|text| {
            schema
                .get("minLength")
                .and_then(Value::as_u64)
                .is_none_or(|min| text.chars().count() >= min as usize)
                && schema
                    .get("enum")
                    .and_then(Value::as_array)
                    .is_none_or(|allowed| allowed.contains(value))
        }),
        Some("integer") => value.as_u64().is_some_and(|number| {
            schema
                .get("minimum")
                .and_then(Value::as_u64)
                .is_none_or(|min| number >= min)
                && schema
                    .get("maximum")
                    .and_then(Value::as_u64)
                    .is_none_or(|max| number <= max)
        }),
        Some("boolean") => value.is_boolean(),
        Some("array") => value
            .as_array()
            .is_some_and(|items| items.iter().all(|item| valid_value(item, &schema["items"]))),
        _ => false,
    }
}
pub(super) fn invalid(text_key: &'static str, message: &'static str) -> ErrorData {
    ErrorData::invalid_params(
        message,
        Some(json!(
            crate::error::AppError::new("mcp.invalid_argument").with_reason(text_key, message)
        )),
    )
}
