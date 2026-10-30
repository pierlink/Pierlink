use super::{RfbProbe, VncStream};
use crate::{error::AppError, security::redact};
use std::time::{Duration, Instant};

/// 消费 stream，在 timeout 总时限内探测 RFB 认证能力并关闭流；不认证、不发送 ClientInit。
/// 探测成功后，界面需另开一条流供 noVNC 完成握手。
pub fn probe_rfb(mut stream: VncStream, timeout: Duration) -> Result<RfbProbe, AppError> {
    let result = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| AppError::new("vnc.probe_timeout"))
        .and_then(|deadline| probe(&mut stream, deadline));
    stream.close();
    result
}

fn read<const N: usize>(stream: &mut VncStream, deadline: Instant) -> Result<[u8; N], AppError> {
    let mut bytes = [0; N];
    let mut offset = 0;
    while offset < N {
        if Instant::now() >= deadline {
            return Err(AppError::new("vnc.probe_timeout"));
        }
        offset += stream
            .reader
            .read_until(&mut bytes[offset..], Some(deadline))?;
    }
    Ok(bytes)
}

fn refused(stream: &mut VncStream, deadline: Instant) -> Result<RfbProbe, AppError> {
    let length = u32::from_be_bytes(read(stream, deadline)?) as usize;
    if length > 4096 {
        return Err(AppError::new("vnc.server_refused").with_reason(
            "vnc.refusal_message_is_too_long",
            "服务器拒绝连接，说明过长",
        ));
    }
    let mut bytes = vec![0; length];
    let mut offset = 0;
    while offset < length {
        offset += stream
            .reader
            .read_until(&mut bytes[offset..], Some(deadline))?;
    }
    let reason = redact(&String::from_utf8_lossy(&bytes), &[])
        .chars()
        .filter(|c| !c.is_control())
        .take(512)
        .collect::<String>();
    Err(AppError::new("vnc.server_refused").with("reason", reason))
}

fn probe(stream: &mut VncStream, deadline: Instant) -> Result<RfbProbe, AppError> {
    let banner = read::<12>(stream, deadline)?;
    let minor = std::str::from_utf8(&banner[8..11])
        .ok()
        .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u16>().ok());
    if &banner[..8] != b"RFB 003." || banner[11] != b'\n' || !minor.is_some_and(|n| n >= 3) {
        let printable: String = banner
            .iter()
            .filter(|b| b.is_ascii_graphic() || **b == b' ')
            .map(|b| *b as char)
            .collect();
        return Err(AppError::new("vnc.not_rfb").with("banner", redact(&printable, &[])));
    }
    stream
        .writer
        .write_until(b"RFB 003.008\n", Some(deadline))?;
    stream.writer.flush_until(deadline)?;
    let mut types = Vec::new();
    if minor.is_some_and(|n| n < 7) {
        let kind = u32::from_be_bytes(read(stream, deadline)?);
        if kind == 0 {
            return refused(stream, deadline);
        }
        types.push(kind);
    } else {
        let count = read::<1>(stream, deadline)?[0];
        if count == 0 {
            return refused(stream, deadline);
        }
        for _ in 0..count {
            types.push(u32::from(read::<1>(stream, deadline)?[0]));
        }
    }
    let mut subtypes = Vec::new();
    if types.contains(&19) {
        stream.writer.write_until(&[19], Some(deadline))?;
        stream.writer.flush_until(deadline)?;
        let version = read::<2>(stream, deadline)?;
        if version != [0, 2] {
            return Err(AppError::new("vnc.server_refused").with_reason(
                "vnc.server_does_not_support_vencrypt_0_2",
                "服务器不支持 VeNCrypt 0.2",
            ));
        }
        stream.writer.write_until(&[0, 2], Some(deadline))?;
        stream.writer.flush_until(deadline)?;
        if read::<1>(stream, deadline)? != [0] {
            return Err(AppError::new("vnc.server_refused").with_reason(
                "vnc.server_rejected_the_vencrypt_version",
                "服务器拒绝 VeNCrypt 版本",
            ));
        }
        let count = read::<1>(stream, deadline)?[0];
        if count == 0 {
            return Err(AppError::new("vnc.server_refused").with_reason(
                "vnc.server_offered_no_vencrypt_subtypes",
                "服务器未提供 VeNCrypt 子类型",
            ));
        }
        for _ in 0..count {
            subtypes.push(u32::from_be_bytes(read(stream, deadline)?));
        }
    }
    let no_auth = types.contains(&1) || subtypes.contains(&1);
    let supported = types.iter().any(|t| [1, 2, 6, 16, 22, 30, 113].contains(t))
        || (types.contains(&19) && subtypes.iter().any(|t| [1, 2, 256].contains(t)));
    if !supported {
        return Err(AppError::new("vnc.tls_only").with("types", format!("{types:?}/{subtypes:?}")));
    }
    Ok(RfbProbe {
        version: String::from_utf8_lossy(&banner).trim_end().into(),
        security_types: types,
        vencrypt_subtypes: subtypes,
        no_auth,
        novnc_supported: supported,
    })
}
