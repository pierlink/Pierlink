//! SFTP v3 的有界二进制编码；不信任服务器给出的长度、类型及状态文本。
use crate::error::AppError;

pub(super) const MAX_PACKET: usize = 1024 * 1024;
pub(super) const CHUNK: usize = 32 * 1024;

pub(super) fn protocol(reason: &str) -> AppError {
    AppError::new("sftp.protocol").with("reason", reason)
}
pub(super) fn string(out: &mut Vec<u8>, value: &[u8]) -> Result<(), AppError> {
    let len = u32::try_from(value.len()).map_err(|_| protocol("字段过长"))?;
    out.extend(len.to_be_bytes());
    out.extend(value);
    Ok(())
}
pub(super) fn path(out: &mut Vec<u8>, value: &str) -> Result<(), AppError> {
    if value.is_empty() || value.contains('\0') || value.len() > MAX_PACKET / 2 {
        return Err(protocol("路径为空、含空字符或过长"));
    }
    string(out, value.as_bytes())
}
pub(super) struct Decoder<'a>(pub &'a [u8]);
impl<'a> Decoder<'a> {
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], AppError> {
        if self.0.len() < n {
            return Err(protocol("响应字段不完整"));
        }
        let (value, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(value)
    }
    pub fn byte(&mut self) -> Result<u8, AppError> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32, AppError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self) -> Result<u64, AppError> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    pub fn string(&mut self) -> Result<&'a [u8], AppError> {
        let len = self.u32()? as usize;
        self.take(len)
    }
    pub fn end(&self) -> Result<(), AppError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(protocol("响应含多余字段"))
        }
    }
    pub fn attrs(&mut self) -> Result<Attrs, AppError> {
        let flags = self.u32()?;
        if flags & !0x8000_000f != 0 {
            return Err(protocol("不支持的属性标志"));
        }
        let size = if flags & 1 != 0 { self.u64()? } else { 0 };
        if flags & 2 != 0 {
            self.take(8)?;
        }
        let mode = if flags & 4 != 0 { self.u32()? } else { 0 };
        let mtime = if flags & 8 != 0 {
            self.u32()?;
            i64::from(self.u32()?)
        } else {
            0
        };
        if flags & 0x8000_0000 != 0 {
            let count = self.u32()?;
            for _ in 0..count {
                self.string()?;
                self.string()?;
            }
        }
        Ok(Attrs { size, mode, mtime })
    }
}
pub(super) struct Attrs {
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
}
pub(super) fn status(input: &mut Decoder<'_>) -> Result<u32, AppError> {
    let code = input.u32()?;
    // 不将服务器任意状态文本带进错误或日志，避免秘密泄漏。
    input.string()?;
    input.string()?;
    input.end()?;
    Ok(code)
}
pub(super) fn status_error(code: u32) -> AppError {
    let (name, reason) = match code {
        2 => ("not_found", "远端路径不存在"),
        3 => ("permission_denied", "远端权限不足"),
        6 | 7 => ("connection_lost", "远端连接已断开"),
        5 => ("protocol", "服务器拒绝协议消息"),
        _ => ("failure", "远端操作失败"),
    };
    AppError::new(format!("sftp.{name}"))
        .with("reason", reason)
        .with("status", code)
}
