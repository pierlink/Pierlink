//! 同步 SFTP v3 客户端；文件传输使用八请求窗口，包与错误输出均有大小上限。
mod codec;
mod operations;
pub mod transfer;
mod transport;

use crate::{error::AppError, process::SshProgram, ssh::args::SshCommand};
use codec::{Attrs, CHUNK, Decoder, path, protocol, status, status_error, string};
use std::collections::VecDeque;
use std::{
    io::{Read, Write},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use transport::{Transport, WINDOW};

/// 按 request.server 的 SFTP 功能选择引擎，返回可交给 SftpSession::open 的命令。
/// 内置模式复用 manager 的批量连接；系统模式只调用 openssh，原参数和凭据行为保持不变。
pub fn command(
    manager: &crate::builtin::ConnectionManager,
    request: crate::builtin::ConnectionRequest<'_>,
    openssh: impl FnOnce() -> Result<SshCommand, AppError>,
) -> Result<SshCommand, AppError> {
    crate::remote::command::for_feature(
        manager,
        request,
        crate::ssh::engine::Feature::Sftp,
        openssh,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct RemoteEntry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Unix 秒；SFTP v3 的时间精度为秒。
    pub mtime: i64,
    pub mode: u32,
    pub target_is_dir: Option<bool>,
}
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
pub struct DirListing {
    pub entries: Vec<RemoteEntry>,
    pub truncated: bool,
    /// 已处理条目中无法用 UTF-8 表示的名字数量；不会返回不可操作的有损名字。
    pub skipped_non_utf8: usize,
}
pub struct SftpSession {
    transport: Transport,
    next_id: u32,
    posix_rename: bool,
    program: SshProgram,
    command: SshCommand,
    timeout: Duration,
}
pub(crate) fn cancelled(cancel: &AtomicBool) -> Result<(), AppError> {
    if cancel.load(Ordering::Relaxed) {
        Err(AppError::new("sftp.cancelled")
            .with_reason("sftp.transfer_cancelled_by_the_user", "用户已取消传输"))
    } else {
        Ok(())
    }
}
pub(crate) fn join_path(parent: &str, name: &str) -> String {
    format!("{}/{name}", parent.trim_end_matches('/'))
}
/// 列目录时一批最多同时问多少个属性：等于连接允许的在途请求数
const STAT_BATCH: usize = WINDOW;

/// 解析 STAT/LSTAT 的回复：属性、不存在（None）或状态错误
fn parse_metadata(
    value: &str,
    kind: u8,
    input: &mut Decoder<'_>,
) -> Result<Option<RemoteEntry>, AppError> {
    match kind {
        105 => {
            let attrs = input.attrs()?;
            input.end()?;
            Ok(Some(entry(
                value.rsplit('/').next().unwrap_or(value).to_owned(),
                attrs,
            )))
        }
        101 => match status(input)? {
            2 => Ok(None),
            code => Err(status_error(code)),
        },
        _ => Err(protocol(
            "sftp.expected_a_file_attributes_response",
            "预期文件属性响应",
        )),
    }
}

fn entry(name: String, attrs: Attrs) -> RemoteEntry {
    let kind = match attrs.mode & 0o170000 {
        0o100000 => EntryKind::File,
        0o040000 => EntryKind::Dir,
        0o120000 => EntryKind::Symlink,
        _ => EntryKind::Other,
    };
    RemoteEntry {
        name,
        kind,
        size: attrs.size,
        mtime: attrs.mtime,
        mode: attrs.mode,
        target_is_dir: None,
    }
}
impl SftpSession {
    /// 用 SFTP 用途 cmd 打开内置批量通道或启动 program；timeout 是每次请求的时限，返回自动回收的会话。
    /// cmd.config_text 的临时配置由调用方在启动前写入、会话结束后删除，与 remote::run 一致。
    pub fn open(
        program: &SshProgram,
        cmd: &SshCommand,
        timeout: Duration,
    ) -> Result<Self, AppError> {
        let mut transport = Transport::open(program, cmd, timeout)?;
        let posix_rename = transport.init()?;
        Ok(Self {
            transport,
            next_id: 0,
            posix_rename,
            program: program.clone(),
            command: cmd.clone(),
            timeout,
        })
    }
    /// 直接启动 path 指定的本机 sftp-server，返回测试会话，不经过 SSH。
    #[cfg(test)]
    pub fn spawn_server(path: &std::path::Path) -> Result<Self, AppError> {
        Self::open(
            &SshProgram { path: path.into() },
            &SshCommand {
                args: vec![],
                env: vec![],
                config_text: None,
                builtin: None,
                askpass: None,
            },
            Duration::from_secs(10),
        )
    }
    fn send(&mut self, kind: u8, body: Vec<u8>) -> Result<u32, AppError> {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let mut packet = vec![kind];
        packet.extend(id.to_be_bytes());
        packet.extend(body);
        self.transport.send(id, packet)?;
        Ok(id)
    }
    fn request(&mut self, kind: u8, body: Vec<u8>) -> Result<Vec<u8>, AppError> {
        let id = self.send(kind, body)?;
        self.transport.receive(id)
    }
    fn reply<T>(
        &mut self,
        packet: &[u8],
        parse: impl FnOnce(u8, &mut Decoder<'_>) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let mut input = Decoder(packet);
        let kind = input.byte()?;
        input.u32()?;
        let result = parse(kind, &mut input);
        if result.as_ref().is_err_and(|e| e.code == "sftp.protocol") {
            self.transport.abort();
        }
        result
    }
    fn ok(&mut self, kind: u8, body: Vec<u8>) -> Result<(), AppError> {
        let packet = self.request(kind, body)?;
        self.reply(&packet, |kind, input| {
            if kind != 101 {
                return Err(protocol("sftp.expected_a_status_response", "预期状态响应"));
            }
            match status(input)? {
                0 => Ok(()),
                code => Err(status_error(code)),
            }
        })
    }
    fn handle(&mut self, kind: u8, body: Vec<u8>) -> Result<Vec<u8>, AppError> {
        let packet = self.request(kind, body)?;
        self.reply(&packet, |kind, input| match kind {
            102 => {
                let handle = input.string()?.to_vec();
                input.end()?;
                Ok(handle)
            }
            101 => Err(status_error(status(input)?)),
            _ => Err(protocol(
                "sftp.expected_a_file_handle_response",
                "预期文件句柄响应",
            )),
        })
    }
    fn with_handle<T>(
        &mut self,
        handle: Vec<u8>,
        operation: impl FnOnce(&mut Self, &[u8]) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let result = operation(self, &handle);
        // 取消、读写失败及读到文件末尾时，先收齐窗口内的响应，再关闭句柄。
        let drained = self.transport.drain();
        let result = result.and_then(|value| drained.map(|()| value));
        let mut body = Vec::new();
        string(&mut body, &handle)?;
        let close = self.ok(4, body);
        if close.is_err() {
            // 句柄关闭失败后不能继续复用连接，以免服务器端资源持续积累。
            self.transport.abort();
        }
        match result {
            Ok(value) => close.map(|()| value),
            Err(e) => Err(e),
        }
    }
    /// 解析远端 path（例如 "."），返回 UTF-8 绝对路径；无损表示不了时返回协议错误。
    pub fn realpath(&mut self, path_value: &str) -> Result<String, AppError> {
        let mut body = Vec::new();
        path(&mut body, path_value)?;
        let packet = self.request(16, body)?;
        self.reply(&packet, |kind, input| {
            if kind == 101 {
                return Err(status_error(status(input)?));
            }
            if kind != 104 || input.u32()? != 1 {
                return Err(protocol(
                    "sftp.invalid_path_resolution_response",
                    "路径解析响应无效",
                ));
            }
            let value = std::str::from_utf8(input.string()?)
                .map_err(|_| protocol("sftp.path_is_not_valid_utf_8", "路径不是 UTF-8"))?
                .to_owned();
            input.string()?;
            input.attrs()?;
            input.end()?;
            if value.contains('\0') {
                return Err(protocol(
                    "sftp.path_contains_null_characters",
                    "路径含空字符",
                ));
            }
            Ok(value)
        })
    }
    fn metadata(&mut self, value: &str, follow: bool) -> Result<Option<RemoteEntry>, AppError> {
        let mut body = Vec::new();
        path(&mut body, value)?;
        let packet = self.request(if follow { 17 } else { 7 }, body)?;
        self.reply(&packet, |kind, input| parse_metadata(value, kind, input))
    }
    /// 一批路径的属性：一组请求先全部发出、再按顺序收回（每组最多 STAT_BATCH 个），
    /// 比一个个问少很多来回。外层错误是连接或协议问题；每项自己的错误（如没有权限）放在对应位置。
    fn metadata_many(
        &mut self,
        values: &[String],
        follow: bool,
    ) -> Result<Vec<Result<Option<RemoteEntry>, AppError>>, AppError> {
        let mut out = Vec::with_capacity(values.len());
        for chunk in values.chunks(STAT_BATCH) {
            let mut ids = Vec::with_capacity(chunk.len());
            for value in chunk {
                let mut body = Vec::new();
                path(&mut body, value)?;
                ids.push(self.send(if follow { 17 } else { 7 }, body)?);
            }
            for (value, id) in chunk.iter().zip(ids) {
                let packet = self.transport.receive(id)?;
                let result = self.reply(&packet, |kind, input| parse_metadata(value, kind, input));
                if result.as_ref().is_err_and(|e| e.code == "sftp.protocol") {
                    return Err(result.err().unwrap_or_else(|| {
                        protocol(
                            "sftp.expected_a_file_attributes_response",
                            "预期文件属性响应",
                        )
                    }));
                }
                out.push(result);
            }
        }
        Ok(out)
    }
    /// 读取 path 的 lstat 属性；不存在返回 None，符号链接保留其自身类型并查询目标是否为目录。
    pub fn stat(&mut self, path: &str) -> Result<Option<RemoteEntry>, AppError> {
        let mut value = self.metadata(path, false)?;
        if let Some(entry) = value.as_mut().filter(|e| e.kind == EntryKind::Symlink) {
            entry.target_is_dir = self.target_is_dir(path)?;
        }
        Ok(value)
    }
    fn target_is_dir(&mut self, path: &str) -> Result<Option<bool>, AppError> {
        match self.metadata(path, true) {
            Ok(target) => Ok(target.map(|e| e.kind == EntryKind::Dir)),
            Err(e) if e.code == "sftp.permission_denied" || e.code == "sftp.failure" => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// 列出 path（包含隐藏文件，排除 . 和 ..）；最多返回 limit 个有效条目，并报告截断及非 UTF-8 跳过数。
    pub fn list_dir(&mut self, value: &str, limit: usize) -> Result<DirListing, AppError> {
        let mut body = Vec::new();
        path(&mut body, value)?;
        let handle = self.handle(11, body)?;
        self.with_handle(handle, |session, handle| {
            let mut result = DirListing {
                entries: vec![],
                truncated: false,
                skipped_non_utf8: 0,
            };
            loop {
                let mut body = Vec::new();
                string(&mut body, handle)?;
                let packet = session.request(12, body)?;
                let batch = session.reply(&packet, |kind, input| {
                    if kind == 101 {
                        return match status(input)? {
                            1 => Ok(None),
                            code => Err(status_error(code)),
                        };
                    }
                    if kind != 104 {
                        return Err(protocol("sftp.expected_a_directory_entries_response", "预期目录条目响应"));
                    }
                    let count = input.u32()?;
                    if count == 0 {
                        return Err(protocol("sftp.server_returned_an_empty_directory_batch_without_ending_the_listing", "服务器返回空目录批次而未结束"));
                    }
                    let mut entries = Vec::new();
                    for _ in 0..count {
                        let name = input.string()?.to_vec();
                        input.string()?;
                        entries.push((name, input.attrs()?));
                    }
                    input.end()?;
                    Ok(Some(entries))
                })?;
                let Some(batch) = batch else {
                    return Ok(result);
                };
                let mut items = Vec::new();
                for (name, attrs) in batch {
                    if name == b"." || name == b".." {
                        continue;
                    }
                    let Ok(name) = String::from_utf8(name) else {
                        result.skipped_non_utf8 += 1;
                        continue;
                    };
                    if name.is_empty() || name.contains(['/', '\0']) {
                        return Err(protocol("sftp.directory_response_contains_an_invalid_name", "目录响应含非法名字"));
                    }
                    items.push(entry(name, attrs));
                }
                // 没带属性的和符号链接要再查：这一批一起问（和逐个调用 stat 的结果相同）
                let need: Vec<usize> = (0..items.len())
                    .filter(|&i| items[i].mode == 0 || items[i].kind == EntryKind::Symlink)
                    .collect();
                let paths: Vec<String> = need
                    .iter()
                    .map(|&i| join_path(value, &items[i].name))
                    .collect();
                let lstats = session.metadata_many(&paths, false)?;
                let mut keep = vec![true; items.len()];
                let mut links = Vec::new();
                for ((&i, full), stat) in need.iter().zip(&paths).zip(lstats) {
                    match stat? {
                        None => keep[i] = false,
                        Some(metadata) => {
                            if metadata.kind == EntryKind::Symlink {
                                links.push((i, full.clone()));
                            }
                            items[i] = metadata;
                        }
                    }
                }
                let targets: Vec<String> = links.iter().map(|(_, p)| p.clone()).collect();
                for ((i, _), target) in links.iter().zip(session.metadata_many(&targets, true)?) {
                    items[*i].target_is_dir = match target {
                        Ok(t) => t.map(|e| e.kind == EntryKind::Dir),
                        Err(e)
                            if e.code == "sftp.permission_denied" || e.code == "sftp.failure" =>
                        {
                            None
                        }
                        Err(e) => return Err(e),
                    };
                }
                for (item, keep) in items.into_iter().zip(keep) {
                    if !keep {
                        continue;
                    }
                    if result.entries.len() == limit {
                        result.truncated = true;
                        return Ok(result);
                    }
                    result.entries.push(item);
                }
            }
        })
    }
    /// 在远端创建 path 目录；权限由服务器的 umask 决定，返回状态错误。
    pub fn mkdir(&mut self, value: &str) -> Result<(), AppError> {
        let mut body = Vec::new();
        path(&mut body, value)?;
        body.extend(0u32.to_be_bytes());
        self.ok(14, body)
    }
    // 返回是否使用了先删除目标的非原子覆盖，供传输报告说明空窗及失败风险。
    fn rename_file(&mut self, from: &str, to: &str, overwrite: bool) -> Result<bool, AppError> {
        let extended = self.posix_rename && overwrite;
        let mut body = Vec::new();
        if extended {
            string(&mut body, b"posix-rename@openssh.com")?;
        }
        path(&mut body, from)?;
        path(&mut body, to)?;
        let mut removed = false;
        if overwrite
            && !extended
            && let Some(target) = self.stat(to)?
        {
            if target.kind == EntryKind::Dir {
                return Err(AppError::new("sftp.failure").with_reason(
                    "sftp.destination_is_a_directory_and_cannot_be_removed",
                    "覆盖目标是目录，不能删除",
                ));
            }
            self.remove_file(to)?;
            removed = true;
        }
        self.ok(if extended { 200 } else { 18 }, body)
            .map(|()| removed)
            .map_err(|error| {
                if removed {
                    error.with_text(
                        "overwrite_note",
                        "sftp.server_lacks_atomic_overwrite_the_old_destination_was_removed_but",
                        "服务器不支持原子覆盖；已删除旧目标，但改名失败",
                    )
                } else {
                    error
                }
            })
    }
    fn cleanup_file(&mut self, path: &str) -> Result<(), AppError> {
        match self.remove_file(path) {
            Err(e) if e.code == "sftp.connection_lost" => {
                // 连接中断时仅重连一次用于清理，不重试传输。服务器不可达时向调用方报告残留路径。
                let mut cleanup = Self::open(
                    &self.program,
                    &self.command,
                    self.timeout.min(Duration::from_secs(2)),
                )?;
                cleanup.remove_file(path)
            }
            result => result,
        }
    }
    /// 删除远端 path 文件或符号链接；不存在返回 sftp.not_found。
    pub fn remove_file(&mut self, value: &str) -> Result<(), AppError> {
        let mut body = Vec::new();
        path(&mut body, value)?;
        self.ok(13, body)
    }
    /// 设置远端 path 的 Unix 秒修改时间 mtime；超出 SFTP v3 范围时返回错误，访问时间设为同值。
    pub fn set_mtime(&mut self, value: &str, mtime: i64) -> Result<(), AppError> {
        let mtime = u32::try_from(mtime).map_err(|_| {
            protocol(
                "sftp.modification_time_exceeds_the_sftp_v3_range",
                "修改时间超出 SFTP 第三版范围",
            )
        })?;
        let mut body = Vec::new();
        path(&mut body, value)?;
        body.extend(8u32.to_be_bytes());
        body.extend(mtime.to_be_bytes());
        body.extend(mtime.to_be_bytes());
        self.ok(9, body)
    }
    /// 以最多八个 32 KiB 请求从 path 读取，按偏移顺序写入 out；回调累计字节，cancel 取消后排空窗口。
    /// 返回实际字节数；任一块失败时停止输出，并收齐已发送响应或中止连接。
    pub fn read_to(
        &mut self,
        value: &str,
        out: &mut dyn Write,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<u64, AppError> {
        cancelled(cancel)?;
        let mut body = Vec::new();
        path(&mut body, value)?;
        body.extend(1u32.to_be_bytes());
        body.extend(0u32.to_be_bytes());
        let handle = self.handle(3, body)?;
        self.with_handle(handle, |session, handle| {
            let mut pending = VecDeque::new();
            let mut next = 0u64;
            let mut written = 0u64;
            loop {
                while pending.len() < WINDOW {
                    cancelled(cancel)?;
                    pending.push_back((session.send_read(handle, next, CHUNK)?, CHUNK));
                    next = next
                        .checked_add(CHUNK as u64)
                        .ok_or_else(|| protocol("sftp.file_offset_overflow", "文件偏移溢出"))?;
                }
                let (id, requested) = pending.pop_front().ok_or_else(|| {
                    protocol("sftp.read_window_is_unexpectedly_empty", "读取窗口意外为空")
                })?;
                let data = session.read_data(id, requested)?;
                cancelled(cancel)?;
                let Some(data) = data else {
                    // 末尾之外的预读也必须校验，不能把窗口中后到的错误误报为成功。
                    for (id, requested) in pending {
                        session.read_data(id, requested)?;
                        cancelled(cancel)?;
                    }
                    return Ok(written);
                };
                out.write_all(&data).map_err(|_| {
                    AppError::new("sftp.failure")
                        .with_reason("sftp.couldn_t_write_to_local_output", "写入本机输出失败")
                })?;
                written = written
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| protocol("sftp.file_offset_overflow", "文件偏移溢出"))?;
                progress(written);
                cancelled(cancel)?;
                // 短块未必是文件末尾：补读缺口，再输出后续偏移，防止丢失中间数据。
                if data.len() < requested {
                    let remaining = requested - data.len();
                    pending.push_front((session.send_read(handle, written, remaining)?, remaining));
                }
            }
        })
    }
    fn read_data(&mut self, id: u32, requested: usize) -> Result<Option<Vec<u8>>, AppError> {
        let packet = self.transport.receive(id)?;
        self.reply(&packet, |kind, input| {
            if kind == 101 {
                return match status(input)? {
                    1 => Ok(None),
                    code => Err(status_error(code)),
                };
            }
            if kind != 103 {
                return Err(protocol(
                    "sftp.expected_a_file_data_response",
                    "预期文件数据响应",
                ));
            }
            let data = input.string()?.to_vec();
            input.end()?;
            if data.is_empty() || data.len() > requested {
                return Err(protocol(
                    "sftp.invalid_file_data_block_size",
                    "文件数据块大小无效",
                ));
            }
            Ok(Some(data))
        })
    }
    fn send_read(&mut self, handle: &[u8], offset: u64, length: usize) -> Result<u32, AppError> {
        let mut body = Vec::new();
        string(&mut body, handle)?;
        body.extend(offset.to_be_bytes());
        body.extend((length as u32).to_be_bytes());
        self.send(5, body)
    }
    /// 把 input 写入 path（创建或截断），mode 是可选 Unix 权限；回调、取消和返回字节数与 read_to 相同。
    pub fn write_from(
        &mut self,
        value: &str,
        input: &mut dyn Read,
        mode: Option<u32>,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<u64, AppError> {
        self.write_file(value, input, mode, progress, cancel, false)
    }
    pub(crate) fn write_file(
        &mut self,
        value: &str,
        input: &mut dyn Read,
        mode: Option<u32>,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
        exclusive: bool,
    ) -> Result<u64, AppError> {
        cancelled(cancel)?;
        let handle = self.open_write(value, mode, exclusive)?;
        self.with_handle(handle, |session, handle| {
            session.write_handle(handle, input, progress, cancel)
        })
    }
    fn open_write(
        &mut self,
        value: &str,
        mode: Option<u32>,
        exclusive: bool,
    ) -> Result<Vec<u8>, AppError> {
        let mut body = Vec::new();
        path(&mut body, value)?;
        body.extend((2u32 | 8 | if exclusive { 32 } else { 16 }).to_be_bytes());
        body.extend(if mode.is_some() { 4u32 } else { 0 }.to_be_bytes());
        if let Some(mode) = mode {
            body.extend((mode & 0o777).to_be_bytes());
        }
        self.handle(3, body)
    }
    fn write_handle(
        &mut self,
        handle: &[u8],
        input: &mut dyn Read,
        progress: &mut dyn FnMut(u64),
        cancel: &AtomicBool,
    ) -> Result<u64, AppError> {
        let mut pending = VecDeque::new();
        let mut next = 0u64;
        let mut done = 0u64;
        let mut ended = false;
        let mut buffer = [0u8; CHUNK];
        loop {
            while !ended && pending.len() < WINDOW {
                cancelled(cancel)?;
                let n = loop {
                    match input.read(&mut buffer) {
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        result => {
                            break result.map_err(|_| {
                                AppError::new("sftp.failure").with_reason(
                                    "sftp.couldn_t_read_local_input",
                                    "读取本机输入失败",
                                )
                            })?;
                        }
                    }
                };
                if n == 0 {
                    ended = true;
                    break;
                }
                cancelled(cancel)?;
                let mut body = Vec::new();
                string(&mut body, handle)?;
                body.extend(next.to_be_bytes());
                string(&mut body, &buffer[..n])?;
                let id = self.send(6, body)?;
                next = next
                    .checked_add(n as u64)
                    .ok_or_else(|| protocol("sftp.file_offset_overflow", "文件偏移溢出"))?;
                pending.push_back((id, next));
            }
            let Some((id, offset)) = pending.pop_front() else {
                return Ok(done);
            };
            let packet = self.transport.receive(id)?;
            self.reply(&packet, |kind, input| {
                if kind != 101 {
                    return Err(protocol("sftp.expected_a_status_response", "预期状态响应"));
                }
                match status(input)? {
                    0 => Ok(()),
                    code => Err(status_error(code)),
                }
            })?;
            done = offset;
            progress(done);
            cancelled(cancel)?;
        }
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod pipeline_tests;

#[cfg(all(test, unix))]
mod sshd_tests;
