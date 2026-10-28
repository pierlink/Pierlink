//! 终端输入在串行门内读屏、检查及发送，跨请求保留尚未提交的行。
use super::*;
use crate::mcp::safety::{is_dangerous, looks_like_password_prompt};
use serde_json::{Value, json};

#[derive(Clone, Default)]
pub(super) struct PendingLine {
    text: String,
    edited: bool,
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
            || input.text.as_ref().is_some_and(|s| s.is_empty())
            || input
                .keys
                .as_ref()
                .is_some_and(|k| k.is_empty() || k.len() > 64)
        {
            return Err(error(
                "mobile.invalid_request",
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
                    "文字中含有不支持的控制字符",
                ));
            }
            let screen = self
                .host
                .terminal_text(id)
                .await
                .map_err(|_| error("mobile.host_failed", "无法读取终端当前状态"))?;
            if looks_like_password_prompt(&screen) {
                return Err(error(
                    "mobile.password_prompt",
                    "终端正在等待密码或验证码，请在电脑上输入",
                ));
            }
            for ch in text.chars() {
                if ch == '\r' || ch == '\n' {
                    submit(&mut pending, &mut dangerous);
                } else {
                    if ch == '\t' {
                        pending.edited = true;
                    }
                    pending.text.push(ch);
                }
            }
        }
        if let Some(keys) = &input.keys {
            for key in keys {
                match key.as_str() {
                    "Enter" | "Ctrl+J" | "Ctrl+M" => submit(&mut pending, &mut dangerous),
                    "Backspace" => {
                        pending.text.pop();
                    }
                    "Ctrl+C" | "Ctrl+U" => pending = PendingLine::default(),
                    "Tab" | "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" | "Home"
                    | "End" => pending.edited = true,
                    "Escape" | "Ctrl+D" => {}
                    _ => return Err(error("mobile.invalid_request", "按键不在手机输入白名单中")),
                }
            }
        }
        if !dangerous.is_empty() && !input.confirmed {
            return Ok(InputResult::Confirm(dangerous));
        }
        if pending.text.len() > 64 * 1024 {
            return Err(error(
                "mobile.invalid_request",
                "尚未提交的终端输入超过六十四 KiB",
            ));
        }
        {
            let lines = lock(&self.pending_lines);
            if lines.len() >= 128 && !lines.contains_key(id) {
                return Err(error("mobile.input_limit", "尚未提交输入的终端数量过多"));
            }
        }
        // 宿主可能已经写入一部分后才报错或被取消，先标记不确定，下一次提交必须核对。
        // 正常成功后再换成准确的剩余行；不能把失败当成“一个字都没有发送”。
        {
            let mut lines = lock(&self.pending_lines);
            lines.entry(id.into()).or_default().edited = true;
        }
        let output = self
            .host
            .terminal_input(id, input)
            .await
            .map_err(|_| error("mobile.host_failed", "桌面终端输入失败"))?;
        let mut lines = lock(&self.pending_lines);
        if pending.text.is_empty() && !pending.edited {
            lines.remove(id);
        } else {
            lines.insert(id.into(), pending);
        }
        Ok(InputResult::Sent(output))
    }
}

fn submit(pending: &mut PendingLine, dangerous: &mut Vec<String>) {
    if pending.edited {
        dangerous.push("终端编辑后的命令无法完整识别，请核对屏幕后确认".into());
    } else if is_dangerous(&pending.text) {
        dangerous.push(pending.text.clone());
    }
    *pending = PendingLine::default();
}

pub(super) fn confirmation(lines: Vec<String>) -> Value {
    json!({"needs_confirm":true,"lines":lines})
}
