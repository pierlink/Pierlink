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
        "Insert",
        "Shift+Tab",
        "Shift+Up",
        "Shift+Down",
        "Shift+Left",
        "Shift+Right",
        "Shift+Home",
        "Shift+End",
        "Shift+Enter",
        "Ctrl+Enter",
        "Alt+Enter",
        "Ctrl+Up",
        "Ctrl+Down",
        "Ctrl+Left",
        "Ctrl+Right",
        "Ctrl+Home",
        "Ctrl+End",
        "Alt+Up",
        "Alt+Down",
        "Alt+Left",
        "Alt+Right",
        "Ctrl+Backspace",
        "Alt+Backspace",
        "Ctrl+Delete",
        "Ctrl+Space",
        "Ctrl+_",
        "Ctrl+]",
        "Ctrl+\\",
        "Ctrl+^",
        "Ctrl+/",
        "WheelUp",
        "WheelDown",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    keys.extend((1..=12).map(|key| format!("F{key}")));
    for prefix in ["Ctrl", "Alt"] {
        keys.extend(
            ('A'..='Z')
                .chain('a'..='z')
                .map(|key| format!("{prefix}+{key}")),
        );
    }
    let wait_timeout = json!({"type":"integer","minimum":1,"maximum":1800,"default":60});
    let sync_timeout = json!({"type":"integer","minimum":1,"maximum":1800,"default":300});
    [
        ("list_targets", "列出入口允许的目标及在线状态；folders 为可用授权目录，unavailable_folders 列出不可用目录的错误码和原因。Windows 目标的 computer 是电脑名：需要用户确认时，确认框弹在这台电脑的 Pierlink 里。几台电脑同时接入时 Windows 目标带 computers：操作都在 current 那台（用户正在用的、或用户指定的）执行，结果的 computer 写明在哪台执行，别的电脑上的终端和任务 ID 以 pc: 开头。", json!({}), json!([])),
        ("run_command", "在目标执行非交互命令，检查 exit_code；退出码取最后一条语句。要拿原生程序的退出码，不要在后面接管道（例如 cargo build 2>&1 | Select-Object -Last 50，构建失败时仍可能返回 0）。返回 stdout、stderr、cwd、shell、truncated、timed_out、duration_ms。Windows 默认 powershell 和用户目录，cwd 必须是 Windows 绝对路径；shell 只能选 list_targets 列出的项。background:true 立即返回 job_id（不受 timeout_secs 限制），用 job_output 读取输出，用 job_stop 结束进程树。入口重启（换令牌、全部停止）会结束后台任务。每入口最多 8 个后台任务，用 list_jobs 查看、job_stop 结束不用的；后台任务目前只支持 Windows，服务器长任务请用 AI 终端。前台输出各保留 256 KiB 头尾，最长 1800 秒。", json!({"target":target,"command":{"type":"string","minLength":1},"cwd":string,"timeout_secs":timeout,"shell":{"type":"string","enum":["powershell","pwsh","cmd","git-bash"]},"background":boolean}), json!(["target","command"])),
        ("open_terminal", "请用户界面打开可见的 AI 终端，返回 terminal_id。cwd 会以一行 cd 的形式回显。", json!({"target":target,"cwd":string,"title":string}), json!(["target"])),
        ("list_terminals", "返回 ai_terminals：AI 打开的、目标获准的终端，含 terminal_id、target（不含地址）和 cwd（未知为 null）。", json!({}), json!([])),
        ("close_terminal", "关闭已登记的 AI 终端，不能关闭用户自己的终端。", json!({"terminal_id":id}), json!(["terminal_id"])),
        ("terminal_send", "发送文字、按键或图片，先发 image_path、再发 text、最后发 keys。keys 的可选值见 schema（Ctrl+字母不区分大小写）；Shift+Enter、Ctrl+Enter 换行不提交，和 Alt+Enter 一样按终端里的程序发合适的字节（和 Pierlink 界面一样，Claude Code、Codex 里是换行；命令提示符会当回车，所以换行键处也按提交审批一次，回车时整段再审批）；WheelUp/WheelDown 是滚一格滚轮，只用于全屏程序（Codex、less、vim 等），普通命令行里更早的输出用 terminal_read 读。text 的 CRLF 和 LF 都转换为 CR 提交。paste:true 把 text 作为一次粘贴发送：换行原样保留、不逐行提交，也不自动回车，要提交再发 keys [\"Enter\"]；适合往 Claude Code、Codex 或 shell 里输入多行内容；程序没开括号粘贴时报错 mcp.paste_unsupported，请改为逐行发送；粘贴的文字不能有 Esc 等控制字符（Tab 和换行可以）。image_path 是 Windows 授权文件夹里的 png/jpg/gif/webp 图片（最大 8 MiB），只用于服务器终端：（要确认时确认之后）传到那台服务器的临时目录（只有自己能读）并把路径粘贴进去（Claude Code 里显示为 [Image #1]），在 text、keys 之前发，返回的 image 是服务器上的路径；本机终端暂不支持。成功只表示已入队，返回 cursor（入队前的输出末尾字节游标）；把 cursor 作为 terminal_wait / terminal_read 的 since，就不会漏掉命令的输出；屏幕在等密码时发送文字会被拒绝，请用户在 Pierlink 里输入，按键仍允许。", json!({"terminal_id":id,"text":string,"paste":boolean,"image_path":id,"keys":{"type":"array","items":{"type":"string","enum":keys}}}), json!(["terminal_id"])),
        ("terminal_read", "从 since 字节游标读取新输出；省略 since 从保留历史的开头读取。tail_bytes 只读最近 N 个原始字节（最多 64 KiB），与 since 同给时取较晚起点。单次最多 64 KiB 文字，停在完整字符和控制序列边界；返回 text、cursor（下次读取的字节偏移）、lost（起点之前的历史已丢失）、more（后面还有未读取的输出）。", json!({"terminal_id":id,"since":number,"tail_bytes":{"type":"integer","minimum":1,"maximum":65536}}), json!(["terminal_id"])),
        ("terminal_screen", "读取终端当前屏幕的文字。", json!({"terminal_id":id}), json!(["terminal_id"])),
        ("terminal_wait", "从 since 字节游标等 pattern 正则匹配或输出安静 quiet_ms；省略 since 从调用时的输出末尾开始。pattern 默认多行模式，^/$ 匹配每行首尾；pattern 和 quiet_ms 都省略时 quiet_ms 默认 2000。返回 matched、text、cursor（下次读取的字节偏移）、lost（历史或等待缓冲已丢失部分输出）、exited（终端进程已退出）、timed_out（达到等待期限，命令可能仍在运行）、notification（终端里的程序响铃或发通知时立即返回：响铃为 bell，Codex 提问、要批准、做完一轮时默认响铃，问题内容看 text 或 terminal_screen；程序发 OSC 9/777 通知时是通知文字；响铃也可能只是程序报错，没等到想要的就再等）；最长 1800 秒。等 Codex、Claude Code 这类 AI 程序时建议同时给 quiet_ms：它们干活时一直刷新进度，停下来通常是在等回答或已做完（Claude Code 默认不发通知，只能这样判断）。", json!({"terminal_id":id,"pattern":string,"quiet_ms":{"type":"integer","minimum":1,"maximum":1800000},"since":number,"timeout_secs":wait_timeout}), json!(["terminal_id"])),
        ("read_file", "读取 Windows 上授权文件夹内的文件，file_path 用 Windows 绝对路径。默认按行读，offset 从 1 开始，limit 默认 2000、最多 5000；返回带行号的文字、编码、换行、修改时间和 sha256；超过 64 MiB 时 sha256 为 null，sha256_skipped_reason 说明原因。byte_offset/max_bytes 改为按字节读取。.png/.jpg/.jpeg/.gif/.webp 图片不超过 2 MiB 时始终返回 MCP 图片内容，可直接查看。", json!({"file_path":id,"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":5000,"default":2000},"byte_offset":number,"max_bytes":{"type":"integer","minimum":1,"maximum":1048576}}), json!(["file_path"])),
        ("read_files", "一次读多个 Windows 文件（file_paths 最多 20 个，都用 Windows 绝对路径），每项和 read_file 默认的按行读取一样，file_path 标明是哪个文件；单个文件出错只在那一项里给 error，不影响别的。limit 是每个文件最多读多少行（默认 2000、最多 5000）；要从中间读或按字节读用 read_file。不超过 2 MiB 的图片和 read_file 一样作为 MCP 图片内容按顺序附在结果后面，那一项的 image 是第几张（从 0 开始）；这次读的图片加起来最多 8 MiB。", json!({"file_paths":{"type":"array","items":id},"limit":{"type":"integer","minimum":1,"maximum":5000,"default":2000}}), json!(["file_paths"])),
        ("write_file", "写入 Windows 文件，file_path 用 Windows 绝对路径，会自动创建缺少的父目录。content 和 content_base64 给一个：content 是文字（最多 5 MiB，保持已有文件的编码、BOM 和换行，不能覆盖二进制文件）；content_base64 是标准 base64 的任意字节（解出后最多 5 MiB，原样写入，可以新建或覆盖二进制文件，比如图片、压缩包、程序）。不能覆盖超过 5 MiB 的已有文件。返回 path、bytes（写入的字节数）和 sha256。expected_sha256 不符时拒绝覆盖；Windows 可执行类型（如 .ps1/.bat/.exe/.dll）和启动文件夹需要确认；授权目录中的 .js/.py/.sh 等开发源码按普通文件处理。", json!({"file_path":id,"content":string,"content_base64":string,"expected_sha256":id}), json!(["file_path"])),
        ("transfer_file", "在 Windows 和一台授权的服务器之间传一个文件（走 Pierlink 的 SFTP 连接，和文件栏的上传下载一样），阻塞到传完。from_target、to_target 一端是 windows、另一端是服务器（list_targets 的名字或 id）；Windows 路径要在授权文件夹里，服务器路径用绝对路径。to_path 是传过去后的完整路径，缺少的父目录自动创建；目标已存在时报错，overwrite:true 时覆盖普通文件。最大 2 GiB，不跟随链接；先写临时文件、传完再改名，出错、取消或超过 timeout_secs（默认 600、最长 1800）时停下并删掉没传完的文件。写到 Windows 按写入规则审批，写到服务器上的 SSH、shell 配置等也要确认。服务器要密码或验证码时报错，请用户在 Pierlink 里登录。返回 bytes、duration_ms。", json!({"from_target":target,"from_path":id,"to_target":target,"to_path":id,"overwrite":{"type":"boolean","default":false},"timeout_secs":{"type":"integer","minimum":1,"maximum":1800,"default":600}}), json!(["from_target","from_path","to_target","to_path"])),
        ("open_path", "在 Windows 上用默认程序打开文件或文件夹（path：Windows 绝对路径，要在授权文件夹里），或用默认浏览器打开网址（url：只允许 http/https），两者给一个。只打开文件夹和文档、图片、音视频、文本、网页、压缩包（如 .pdf/.docx/.png/.mp4/.txt/.json/.html/.zip）；别的类型（脚本、程序、快捷方式、安装包、带宏的文档等）直接拒绝，要运行程序请用 run_command。需要入口允许在 Windows 上执行命令（打开会启动程序）。不等程序退出，返回 opened。", json!({"path":id,"url":id}), json!([])),
        ("make_dir", "创建 Windows 授权范围内的目录及父目录，已有目录成功；不经过链接或目录联接。按写入规则审批。", json!({"path":id}), json!(["path"])),
        ("delete_path", "删除 Windows 授权范围内的文件或空目录；recursive:true 才删除非空目录。只删除链接本身。按写入规则审批，删除目录在只问危险操作模式下也需确认。", json!({"path":id,"recursive":{"type":"boolean","default":false}}), json!(["path"])),
        ("rename_path", "重命名 Windows 授权范围内的路径，两端都须授权；overwrite 默认为 false，目标存在时报错。按写入规则审批，链接只移动链接本身；目标父目录须已存在。", json!({"from":id,"to":id,"overwrite":{"type":"boolean","default":false}}), json!(["from","to"])),
        ("copy_path", "在 Windows 授权文件夹里复制文件或目录，source、destination 用 Windows 绝对路径；destination 是复制后的完整路径（不是放进哪个目录），缺少的父目录自动创建。目录递归复制；不跟随链接和目录联接，遇到的跳过并在 skipped 里列出。目标已存在时报错；overwrite:true 时覆盖同名文件、合并进已有目录（目标里多出来的不删，类型不同时报错）。总量最多 2 GiB、10 万项，超了报错、什么都不复制。按写入规则审批：目标里有脚本、程序、启动文件夹、SSH/Git/shell 配置等要确认的项时确认框会列出。返回 files、dirs、bytes、skipped、skipped_count；中途取消时已复制的文件保留。", json!({"source":id,"destination":id,"overwrite":{"type":"boolean","default":false}}), json!(["source","destination"])),
        ("edit_file", "精确替换 Windows 文件中的文字，file_path 用 Windows 绝对路径。old_string 必须唯一，replace_all 可替换所有匹配；保持原编码、BOM 和换行。一个文件要改好几处时用 edits（最多 100 处，和 old_string/new_string 二选一）：按顺序改，后一处在前一处改完的内容上找，全部成功才写入，任何一处找不到或不唯一时文件不动、错误里写明第几处。返回 replacements（总共替换的次数）和 sha256。可用 expected_sha256 检查读后变化。", json!({"file_path":id,"old_string":id,"new_string":string,"replace_all":boolean,"edits":{"type":"array","items":{"type":"object","properties":{"old_string":id,"new_string":string,"replace_all":boolean},"required":["old_string","new_string"]}},"expected_sha256":id}), json!(["file_path"])),
        ("search", "在 Windows 授权文件夹中用 Rust regex 搜索，path 用 Windows 绝对路径，省略时搜索全部授权文件夹。遵守 .gitignore，不跟随链接；跳过二进制及超过 10 MiB 的文件。content 模式按文件分组为 {path, encoding, line_ending, lines: [{line, text, match}]}；-n:false 省略 line，offset/head_limit 仍按行计。结果按路径排序，最多扫描 20000 项、256 MiB、10 秒，输出最多 256 KiB；达到限制返回 truncated。", json!({"pattern":string,"path":id,"glob":id,"type":id,"output_mode":{"type":"string","enum":["content","files_with_matches","count"],"default":"files_with_matches"},"-i":boolean,"-n":boolean,"multiline":boolean,"-A":{"type":"integer","minimum":0,"maximum":5000},"-B":{"type":"integer","minimum":0,"maximum":5000},"-C":{"type":"integer","minimum":0,"maximum":5000},"head_limit":{"type":"integer","minimum":1,"maximum":10000,"default":250},"offset":number}), json!(["pattern"])),
        ("find_files", "在 Windows 授权文件夹中按 glob 找文件，path 用 Windows 绝对路径，省略时搜索全部授权文件夹。按修改时间从新到旧返回路径；遵守 .gitignore，include_ignored 可关闭忽略规则；不跟随链接，达到扫描或输出限制返回 truncated。", json!({"pattern":id,"path":id,"head_limit":{"type":"integer","minimum":1,"maximum":10000,"default":250},"include_ignored":boolean}), json!(["pattern"])),
        ("job_output", "读取当前入口后台任务的新输出，since 是字节游标，省略从保留输出开头读。单次最多 64 KiB，返回 stdout、stderr、cursor（下次读取位置）、more（还有未读取输出）、lost（更早输出已丢失）、running、exit_code、timed_out 和 stop_reason（timeout 超时、stopped 主动停止、entry_closed 入口关闭、permission_revoked 目标权限收回；正常完成为 null）。输出仅保留最近 1 MiB。", json!({"job_id":id,"since":number}), json!(["job_id"])),
        ("job_stop", "结束当前入口后台任务的整棵进程树。", json!({"job_id":id}), json!(["job_id"])),
        ("list_jobs", "按开始时间从早到晚列出当前入口的后台任务，包含 command（前 200 个字符）、started_ms（Unix 毫秒）和 stop_reason；完成的任务保留 30 分钟。", json!({}), json!([])),
        ("list_dir", "列出 Windows 授权文件夹内容，path 使用 Windows 绝对路径；每项含 size、mtime_ms、is_dir，最多 2000 项。", json!({"path":id}), json!(["path"])),
        ("sync_status", "查询允许访问的同步任务状态、待同步字节和各服务器完成度；task 为任务名或 ID。", json!({"task":id}), json!([])),
        ("sync_wait", "先扫描本机改动再等待允许访问的目标同步完成，改完文件后先等待再编译。返回 completed（同步已完成）、timed_out（达到等待期限）、reason（未完成原因）、errors（错误列表）和 tasks（各任务及服务器状态）。任务发生错误、暂停或服务器离线时立即返回。", json!({"task":id,"timeout_secs":sync_timeout}), json!([])),
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
                return Err(missing(field));
            }
        }
    }
    if name == "edit_file" {
        check_edits(args)?;
    }
    for (key, value) in args {
        let Some(schema) = properties.get(key) else {
            let available = properties
                .as_object()
                .into_iter()
                .flat_map(|p| p.keys())
                .cloned()
                .collect::<Vec<_>>()
                .join("、");
            return Err(parameter_error(
                crate::error::AppError::new("mcp.invalid_argument")
                    .with_reason(
                        "mcp.unknown_parameter",
                        format!("未知参数 {key}，可用的有 {available}"),
                    )
                    .with("parameter", key)
                    .with("allowed", available),
            ));
        };
        if !valid_value(value, schema) {
            return Err(parameter_error(value_error(key, schema)));
        }
    }
    if name == "terminal_send"
        && !args.contains_key("text")
        && !args.contains_key("image_path")
        && args
            .get("keys")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        return Err(invalid(
            "mcp.text_or_keys_is_required",
            "需要 text、keys 或 image_path",
        ));
    }
    if name == "terminal_send" && args.get("paste").and_then(Value::as_bool) == Some(true) {
        match args.get("text").and_then(Value::as_str) {
            Some(text) if !text.is_empty() => {
                if !super::terminal::pastable(text) {
                    return Err(invalid(
                        "mcp.paste_text_has_control_characters",
                        "粘贴的文字不能有 Esc 等控制字符（Tab 和换行可以）",
                    ));
                }
            }
            _ => {
                return Err(invalid(
                    "mcp.paste_needs_text",
                    "paste:true 需要非空的 text",
                ));
            }
        }
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
    if name == "read_files"
        && args
            .get("file_paths")
            .and_then(Value::as_array)
            .is_some_and(|paths| paths.is_empty() || paths.len() > 20)
    {
        return Err(invalid(
            "mcp.file_paths_must_have_1_to_20_items",
            "file_paths 要有 1–20 个路径",
        ));
    }
    if name == "open_path" && args.contains_key("path") == args.contains_key("url") {
        return Err(invalid(
            "mcp.give_either_path_or_url",
            "path 和 url 要给一个，也只能给一个",
        ));
    }
    if name == "write_file" && args.contains_key("content") == args.contains_key("content_base64") {
        return Err(invalid(
            "mcp.give_either_content_or_content_base64",
            "content 和 content_base64 要给一个，也只能给一个",
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
fn missing(field: &str) -> ErrorData {
    parameter_error(
        crate::error::AppError::new("mcp.invalid_argument")
            .with_reason(
                "mcp.required_parameter_is_missing",
                format!("缺少必需参数 {field}"),
            )
            .with("parameter", field),
    )
}

/// edit_file 要么给 old_string、new_string（一处修改），要么给 edits（多处修改），不能混着给
fn check_edits(args: &JsonObject) -> Result<(), ErrorData> {
    let single = ["old_string", "new_string", "replace_all"]
        .iter()
        .any(|key| args.contains_key(*key));
    match args.get("edits").and_then(Value::as_array) {
        Some(_) if single => Err(invalid(
            "mcp.edits_cannot_be_combined_with_old_string",
            "edits 和 old_string、new_string、replace_all 不能一起给",
        )),
        Some(edits) if edits.is_empty() || edits.len() > 100 => Err(invalid(
            "mcp.edits_must_have_1_to_100_items",
            "edits 要有 1–100 处修改",
        )),
        Some(_) => Ok(()),
        None if args.contains_key("edits") => Ok(()),
        None => match ["old_string", "new_string"]
            .into_iter()
            .find(|key| !args.contains_key(*key))
        {
            Some(field) => Err(missing(field)),
            None => Ok(()),
        },
    }
}

fn parameter_error(error: crate::error::AppError) -> ErrorData {
    ErrorData::invalid_params(
        error.params.get("reason").cloned().unwrap_or_default(),
        Some(json!(error)),
    )
}

fn value_error(key: &str, schema: &Value) -> crate::error::AppError {
    let error = crate::error::AppError::new("mcp.invalid_argument").with("parameter", key);
    if schema["type"] == "array" {
        let mut item = value_error(key, &schema["items"]);
        let constraint = item.params.get("reason").cloned().unwrap_or_default();
        let constraint_key = item.params.get("reason_key").cloned().unwrap_or_default();
        item = item
            .with_reason(
                "mcp.parameter_array",
                format!("{key} 必须是数组，每项要求：{constraint}"),
            )
            .with("constraint", constraint)
            .with("constraint_key", constraint_key);
        return item;
    }
    if let Some(allowed) = schema["enum"].as_array() {
        let allowed = allowed
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("、");
        return error
            .with_reason(
                "mcp.parameter_enum",
                format!("{key} 必须是以下值之一：{allowed}"),
            )
            .with("allowed", allowed);
    }
    match schema["type"].as_str() {
        Some("object") => {
            let names = |value: &Value| {
                value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("、")
            };
            let required = names(&schema["required"]);
            let fields = schema["properties"]
                .as_object()
                .into_iter()
                .flat_map(|p| p.keys())
                .cloned()
                .collect::<Vec<_>>()
                .join("、");
            error
                .with_reason(
                    "mcp.parameter_object",
                    format!("对象，要有 {required}，可用的字段有 {fields}"),
                )
                .with("required", required)
                .with("fields", fields)
        }
        Some("integer") => {
            let min = schema["minimum"].as_u64().unwrap_or(0);
            let max = schema["maximum"].as_u64().unwrap_or(u64::MAX);
            error
                .with_reason(
                    "mcp.parameter_integer_range",
                    format!("{key} 必须是 {min}–{max} 的整数"),
                )
                .with("minimum", min)
                .with("maximum", max)
        }
        Some("string") => {
            let min = schema["minLength"].as_u64().unwrap_or(0);
            error
                .with_reason(
                    "mcp.parameter_string",
                    format!("{key} 必须是字符串，至少 {min} 个字符"),
                )
                .with("minimum", min)
        }
        _ => error.with_reason(
            "mcp.parameter_boolean",
            format!("{key} 必须是布尔值 true 或 false"),
        ),
    }
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
        Some("object") => value.as_object().is_some_and(|object| {
            schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .all(|key| object.contains_key(key))
                && object.iter().all(|(key, value)| {
                    schema["properties"]
                        .get(key)
                        .is_some_and(|field| valid_value(value, field))
                })
        }),
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 参数错误指出名称和约束且不回显传入值() {
        for (name, args, expected) in [
            (
                "run_command",
                json!({"target":"windows","command":"ok","timeout_secs":1801}),
                "timeout_secs 必须是 1–1800 的整数",
            ),
            (
                "run_command",
                json!({"target":"windows","command":"ok","shell":"secret"}),
                "shell 必须是以下值之一：powershell、pwsh、cmd、git-bash",
            ),
            (
                "job_output",
                json!({"job_id":"id","cursor":0}),
                "未知参数 cursor，可用的有 job_id、since",
            ),
            ("job_output", json!({}), "缺少必需参数 job_id"),
            (
                "terminal_send",
                json!({"terminal_id":"id","keys":["secret"]}),
                "keys 必须是数组",
            ),
        ] {
            let error = validate(name, args.as_object().unwrap()).unwrap_err();
            assert!(error.message.contains(expected), "{error:?}");
            assert!(!error.message.contains("secret"));
            let data = error.data.unwrap();
            assert!(data["params"]["parameter"].is_string());
            assert!(data["params"]["reason_key"].is_string());
        }
    }

    #[test]
    fn 编辑给一处或多处_不能混着给_每处要有新旧文字() {
        let check = |args: Value| validate("edit_file", args.as_object().unwrap());
        assert!(check(json!({"file_path":"C:/a","old_string":"a","new_string":"b"})).is_ok());
        assert!(check(json!({"file_path":"C:/a","edits":[{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"","replace_all":true}]})).is_ok());
        for (args, key) in [
            (
                json!({"file_path":"C:/a","old_string":"a","edits":[{"old_string":"a","new_string":"b"}]}),
                "mcp.edits_cannot_be_combined_with_old_string",
            ),
            (
                json!({"file_path":"C:/a","edits":[]}),
                "mcp.edits_must_have_1_to_100_items",
            ),
            (
                json!({"file_path":"C:/a","old_string":"a"}),
                "mcp.required_parameter_is_missing",
            ),
            (
                json!({"file_path":"C:/a","edits":[{"old_string":"a"}]}),
                "mcp.parameter_array",
            ),
            (
                json!({"file_path":"C:/a","edits":[{"old_string":"a","new_string":"b","x":1}]}),
                "mcp.parameter_array",
            ),
            (
                json!({"file_path":"C:/a","edits":[{"old_string":"","new_string":"b"}]}),
                "mcp.parameter_array",
            ),
        ] {
            let error = check(args).unwrap_err();
            assert_eq!(error.data.unwrap()["params"]["reason_key"], key);
        }
        let error = check(json!({"file_path":"C:/a","edits":["x"]})).unwrap_err();
        assert!(
            error
                .message
                .contains("要有 old_string、new_string，可用的字段有 "),
            "{error:?}"
        );
        let many: Vec<_> = (0..101)
            .map(|_| json!({"old_string":"a","new_string":"b"}))
            .collect();
        let error = check(json!({"file_path":"C:/a","edits":many})).unwrap_err();
        assert_eq!(
            error.data.unwrap()["params"]["reason_key"],
            "mcp.edits_must_have_1_to_100_items"
        );
    }

    #[test]
    fn 写文件要给_content_或_content_base64_中的一个() {
        for args in [
            json!({"file_path":"C:/a.bin"}),
            json!({"file_path":"C:/a.bin","content":"x","content_base64":"eA=="}),
        ] {
            let error = validate("write_file", args.as_object().unwrap()).unwrap_err();
            assert_eq!(
                error.data.unwrap()["params"]["reason_key"],
                "mcp.give_either_content_or_content_base64"
            );
        }
        for args in [
            json!({"file_path":"C:/a.bin","content_base64":"eA=="}),
            json!({"file_path":"C:/a.txt","content":""}),
        ] {
            assert!(validate("write_file", args.as_object().unwrap()).is_ok());
        }
    }

    #[test]
    fn 粘贴要有文字且不能带控制字符() {
        for (args, key) in [
            (
                json!({"terminal_id":"id","paste":true,"keys":["Enter"]}),
                "mcp.paste_needs_text",
            ),
            (
                json!({"terminal_id":"id","paste":true,"text":""}),
                "mcp.paste_needs_text",
            ),
            (
                json!({"terminal_id":"id","paste":true,"text":"a\u{1b}[201~\rrm -rf /"}),
                "mcp.paste_text_has_control_characters",
            ),
        ] {
            let error = validate("terminal_send", args.as_object().unwrap()).unwrap_err();
            assert_eq!(error.data.unwrap()["params"]["reason_key"], key);
        }
        for args in [
            json!({"terminal_id":"id","paste":true,"text":"a\n\tb\r\n"}),
            json!({"terminal_id":"id","paste":false,"text":"a\u{1b}[A"}),
        ] {
            assert!(validate("terminal_send", args.as_object().unwrap()).is_ok());
        }
    }
}
