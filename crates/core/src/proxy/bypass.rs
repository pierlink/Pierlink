use std::net::IpAddr;

fn normalized(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host)
        .trim_end_matches('.')
}

/// 判断 host 是否为回环地址；接受 localhost、IPv4、IPv6 及映射到 IPv4 回环的 IPv6。
/// 忽略 localhost 的大小写、末尾的点和 IPv6 方括号；不解析域名，其他输入返回 false。
pub fn loopback(host: &str) -> bool {
    let host = normalized(host);
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| match ip {
            IpAddr::V4(ip) => ip.is_loopback(),
            IpAddr::V6(ip) => {
                ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
            }
        })
}

pub(super) fn matches(host: &str, rules: &[String]) -> bool {
    let host = normalized(host).to_lowercase();
    rules.iter().any(|rule| {
        let rule = rule.trim().to_lowercase();
        if rule == "<local>" {
            return !host.contains('.') && host.parse::<IpAddr>().is_err();
        }
        if let Some((network, prefix)) = rule.split_once('/') {
            return cidr(&host, network, prefix);
        }
        wildcard(&host, normalized(&rule))
    })
}

fn cidr(host: &str, network: &str, prefix: &str) -> bool {
    match (
        host.parse::<IpAddr>(),
        network.parse::<IpAddr>(),
        prefix.parse::<u32>(),
    ) {
        (Ok(IpAddr::V4(ip)), Ok(IpAddr::V4(net)), Ok(bits @ 0..=32)) => {
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            u32::from(ip) & mask == u32::from(net) & mask
        }
        (Ok(IpAddr::V6(ip)), Ok(IpAddr::V6(net)), Ok(bits @ 0..=128)) => {
            let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
            u128::from(ip) & mask == u128::from(net) & mask
        }
        _ => false,
    }
}

// WinINET 通配使用 * 和 ?；不把模式编译为正则，避免复杂模式导致回溯失控。
fn wildcard(host: &str, pattern: &str) -> bool {
    let host: Vec<_> = host.chars().collect();
    let pattern: Vec<_> = pattern.chars().collect();
    let (mut h, mut p, mut star, mut retry) = (0, 0, None, 0);
    while h < host.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == host[h]) {
            h += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            retry = h;
        } else if let Some(s) = star {
            retry += 1;
            h = retry;
            p = s + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}
