//! 终端输入在串行门内读屏、检查及发送，跨请求保留尚未提交的行。
use super::*;
use crate::mcp::safety::{is_dangerous, looks_like_password_prompt};
use serde_json::{Value, json};

#[derive(Clone, Default)]
pub(super) struct PendingLine {
    text: String,
    pub(super) edited: bool,
}
impl Drop for PendingLine {
    fn drop(&mut self) {
        self.text.zeroize();
    }
}

pub(super) enum InputResult {
    Sent(Value),
    Confirm(Vec<String>),
}

impl Shared {
    pub(super) async fn input(
        &self,
        id: &str,
        input: TerminalInput,
    ) -> Result<InputResult, AppError> {
        if input.text.is_some() == input.keys.is_some()
            || (input.paste && input.text.is_none())
            || input.text.as_ref().is_some_and(|s| s.is_empty())
            || input
                .keys
                .as_ref()
                .is_some_and(|k| k.is_empty() || k.len() > 64)
        {
            return Err(error(
                "mobile.invalid_request",
                "mobile.provide_either_nonempty_text_or_a_key_array",
                "输入必须包含非空文字或按键数组，且不能同时包含两者",
            ));
        }
        let _guard = self.input_gate.lock().await;
        let mut pending = lock(&self.pending_lines)
            .get(id)
            .cloned()
            .unwrap_or_default();
        let mut dangerous = Vec::new();
        if let Some(text) = &input.text {
            // ESC、控制字符不能伪装成文字来输入口令或绕过按键白名单。
            if text
                .chars()
                .any(|c| c.is_control() && c != '\r' && c != '\n' && c != '\t')
            {
                return Err(error(
                    "mobile.invalid_request",
                    "mobile.text_contains_unsupported_control_characters",
                    "文字中含有不支持的控制字符",
                ));
            }
            self.check_password(id).await?;
            if input.paste {
                let body = text.strip_suffix('\r').unwrap_or(text);
                if body.contains('\r') {
                    return Err(error(
                        "mobile.invalid_request",
                        "mobile.paste_carriage_return_must_be_last",
                        "粘贴正文只允许在末尾附带一个回车，正文换行请使用换行符",
                    ));
                }
                pending.text.push_str(body);
                dangerous.extend(dangerous_lines(&pending.text));
                if text.ends_with('\r') {
                    if pending.edited {
                        self.check_edited(id, &mut dangerous).await;
                    }
                    pending = PendingLine::default();
                }
            } else {
                for ch in text.chars() {
                    if ch == '\r' || ch == '\n' {
                        self.submit(id, &mut pending, &mut dangerous).await;
                    } else {
                        if ch == '\t' {
                            pending.edited = true;
                        }
                        pending.text.push(ch);
                    }
                }
            }
        }
        if let Some(keys) = &input.keys {
            for key in keys {
                match key.as_str() {
                    "Enter" | "Ctrl+J" | "Ctrl+M" => {
                        self.submit(id, &mut pending, &mut dangerous).await;
                    }
                    "Backspace" => {
                        pending.text.pop();
                    }
                    "Ctrl+C" | "Ctrl+U" => pending = PendingLine::default(),
                    "Tab" | "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" | "Home"
                    | "End" => pending.edited = true,
                    "Escape" | "Shift+Tab" | "Ctrl+D" => {}
                    _ => {
                        return Err(error(
                            "mobile.invalid_request",
                            "mobile.key_is_not_allowed_for_mobile_input",
                            "按键不在手机输入白名单中",
                        ));
                    }
                }
            }
        }
        if !dangerous.is_empty() && !input.confirmed {
            return Ok(InputResult::Confirm(dangerous));
        }
        if pending.text.len() > 64 * 1024 {
            return Err(error(
                "mobile.invalid_request",
                "mobile.pending_terminal_input_exceeds_64_kib",
                "尚未提交的终端输入超过六十四 KiB",
            ));
        }
        {
            let lines = lock(&self.pending_lines);
            if lines.len() >= 128 && !lines.contains_key(id) {
                return Err(error(
                    "mobile.input_limit",
                    "mobile.too_many_terminals_have_pending_input",
                    "尚未提交输入的终端数量过多",
                ));
            }
        }
        // 宿主可能已经写入一部分后才报错或被取消，先标记不确定，下一次提交必须核对。
        // 正常成功后再换成准确的剩余行；不能把失败当成“一个字都没有发送”。
        {
            let mut lines = lock(&self.pending_lines);
            lines.entry(id.into()).or_default().edited = true;
        }
        let output = self.host.terminal_input(id, input).await.map_err(|_| {
            error(
                "mobile.host_failed",
                "mobile.desktop_terminal_input_failed",
                "桌面终端输入失败",
            )
        })?;
        let mut lines = lock(&self.pending_lines);
        if pending.text.is_empty() && !pending.edited {
            lines.remove(id);
        } else {
            lines.insert(id.into(), pending);
        }
        Ok(InputResult::Sent(output))
    }

    pub(super) async fn check_password(&self, id: &str) -> Result<(), AppError> {
        let screen = self.host.terminal_text(id).await.map_err(|_| {
            error(
                "mobile.host_failed",
                "mobile.couldn_t_read_the_current_terminal_state",
                "无法读取终端当前状态",
            )
        })?;
        if looks_like_password_prompt(&screen) {
            return Err(error(
                "mobile.password_prompt",
                "mobile.terminal_is_waiting_for_a_password_or_verification_code_enter",
                "终端正在等待密码或验证码，请在电脑上输入",
            ));
        }
        Ok(())
    }

    async fn check_edited(&self, id: &str, dangerous: &mut Vec<String>) {
        match self.host.terminal_text(id).await {
            Ok(screen) => {
                let line = screen
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("");
                let command = without_prompt(line);
                if is_dangerous(command) {
                    dangerous.push(command.to_owned());
                }
            }
            Err(_) => dangerous.push("无法读取终端编辑后的命令，请核对屏幕后确认".into()),
        }
    }

    async fn submit(&self, id: &str, pending: &mut PendingLine, dangerous: &mut Vec<String>) {
        if pending.edited {
            self.check_edited(id, dangerous).await;
        } else {
            dangerous.extend(dangerous_lines(&pending.text));
        }
        *pending = PendingLine::default();
    }
}

fn without_prompt(line: &str) -> &str {
    let line = line.trim_start();
    if is_dangerous(line) {
        return line;
    }
    if line.starts_with("PS ")
        && let Some((_, command)) = line.split_once("> ")
    {
        return command;
    }
    for prefix in ["$ ", "# ", "> ", "% "] {
        if let Some(command) = line.strip_prefix(prefix) {
            return command;
        }
    }
    // 路径或用户名提示符只去掉第一个标记，不能吞掉命令参数里的提示符样文字。
    for marker in ["$ ", "# ", "% "] {
        if let Some((_, command)) = line.split_once(marker) {
            return command;
        }
    }
    line
}

fn dangerous_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<_> = text
        .split(['\r', '\n'])
        .filter(|line| is_dangerous(line))
        .map(str::to_owned)
        .collect();
    // 包装命令或续行可能只有合起来才命中，仍须检查整个输入。
    if lines.is_empty() && is_dangerous(text) {
        lines.push(text.to_owned());
    }
    lines
}

pub(super) fn confirmation(lines: Vec<String>) -> Value {
    json!({"needs_confirm":true,"lines":lines})
}
