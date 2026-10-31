use super::SystemProxy;
#[cfg(any(windows, test))]
use super::{ProxyKind, ProxyServer};

/// 无参数；现读 Windows 当前用户的 Internet Settings，其他系统返回 None；PAC 不执行。
pub fn system_proxy() -> SystemProxy {
    #[cfg(windows)]
    {
        parse(
            read_dword("ProxyEnable") != 0,
            &read_string("ProxyServer"),
            &read_string("ProxyOverride"),
            &read_string("AutoConfigURL"),
        )
    }
    #[cfg(not(windows))]
    SystemProxy::None
}

#[cfg(any(windows, test))]
pub(super) fn parse(enabled: bool, server: &str, bypass: &str, pac: &str) -> SystemProxy {
    if !pac.trim().is_empty() {
        return SystemProxy::Pac {
            url: pac.trim().into(),
        };
    }
    if !enabled {
        return SystemProxy::None;
    }
    let mut chosen = None;
    if server.contains('=') {
        for (protocol, kind) in [
            ("https", ProxyKind::Http),
            ("http", ProxyKind::Http),
            ("socks", ProxyKind::Socks5),
        ] {
            if let Some((_, address)) = server
                .split(';')
                .filter_map(|s| s.split_once('='))
                .find(|(key, _)| key.trim().eq_ignore_ascii_case(protocol))
            {
                chosen = endpoint(address.trim(), kind);
                // 优先协议配置无效时不悄悄改走其他代理。
                break;
            }
        }
    } else {
        chosen = endpoint(server.trim(), ProxyKind::Http);
    }
    match chosen {
        Some(server) => SystemProxy::Server {
            server,
            bypass: bypass
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
        },
        None => SystemProxy::None,
    }
}

#[cfg(any(windows, test))]
fn endpoint(value: &str, kind: ProxyKind) -> Option<ProxyServer> {
    let (host, port) = value.rsplit_once(':')?;
    let host = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    let server = ProxyServer {
        kind,
        host: host.into(),
        port: port.parse().ok()?,
    };
    pierlink_ssh_engine::proxy::validate(&server, "example.com", 22).ok()?;
    Some(server)
}

#[cfg(windows)]
fn read_value(name: &str, flags: u32, data: &mut [u8]) -> bool {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RegGetValueW};
    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut size = data.len() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            name.as_ptr(),
            flags,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        ) == 0
    }
}

#[cfg(windows)]
fn read_dword(name: &str) -> u32 {
    let mut data = [0; 4];
    if read_value(
        name,
        windows_sys::Win32::System::Registry::RRF_RT_REG_DWORD,
        &mut data,
    ) {
        u32::from_le_bytes(data)
    } else {
        0
    }
}

#[cfg(windows)]
fn read_string(name: &str) -> String {
    let mut data = vec![0; 64 * 1024];
    if !read_value(
        name,
        windows_sys::Win32::System::Registry::RRF_RT_REG_SZ,
        &mut data,
    ) {
        return String::new();
    }
    let words: Vec<_> = data
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .take_while(|w| *w != 0)
        .collect();
    String::from_utf16_lossy(&words)
}
