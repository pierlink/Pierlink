use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use ssh_key::encoding::base64::{Base64, Encoding};

pub(super) enum Patterns {
    Plain(Vec<String>),
    Hashed { salt: Vec<u8>, hash: Vec<u8> },
}

impl Patterns {
    pub(super) fn parse(text: &str) -> Result<Self, ()> {
        if let Some(hashed) = text.strip_prefix("|1|") {
            let (salt, hash) = hashed.split_once('|').ok_or(())?;
            let salt = Base64::decode_vec(salt).map_err(|_| ())?;
            let hash = Base64::decode_vec(hash).map_err(|_| ())?;
            if salt.len() != 20 || hash.len() != 20 {
                return Err(());
            }
            return Ok(Self::Hashed { salt, hash });
        }
        let patterns: Vec<_> = text.split(',').map(str::to_owned).collect();
        if patterns.iter().any(|p| {
            p.is_empty()
                || p == "!"
                || p.contains('|')
                || p.strip_prefix('!').unwrap_or(p).starts_with('!')
        }) {
            return Err(());
        }
        Ok(Self::Plain(patterns))
    }

    pub(super) fn matches(&self, host: &str) -> bool {
        match self {
            Self::Hashed { salt, hash } => Hmac::<Sha1>::new_from_slice(salt)
                .map(|mac| mac.chain_update(host.as_bytes()).verify_slice(hash).is_ok())
                .unwrap_or(false),
            Self::Plain(patterns) => {
                let mut matched = false;
                for pattern in patterns {
                    if let Some(negative) = pattern.strip_prefix('!') {
                        if wildcard(host.as_bytes(), negative.as_bytes()) {
                            return false;
                        }
                    } else if wildcard(host.as_bytes(), pattern.as_bytes()) {
                        matched = true;
                    }
                }
                matched
            }
        }
    }
}

// 迭代匹配，避免不可信的星号模式导致递归爆栈；方括号在 known_hosts 中是普通字符。
fn wildcard(host: &[u8], pattern: &[u8]) -> bool {
    let (mut h, mut p) = (0, 0);
    let mut star = None;
    let mut retry = 0;
    while h < host.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&host[h])) {
            h += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = h;
        } else if let Some(position) = star {
            retry += 1;
            h = retry;
            p = position + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
