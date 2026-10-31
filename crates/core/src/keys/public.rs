use sha2::{Digest, Sha256};

pub(super) struct PublicKey {
    pub kind: String,
    pub data: String,
    pub comment: String,
    pub fingerprint: String,
}
const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
fn decode(value: &str) -> Option<Vec<u8>> {
    let mut result = Vec::new();
    let (mut bits, mut n) = (0u32, 0);
    for byte in value.trim_end_matches('=').bytes() {
        bits = (bits << 6) | B64.iter().position(|b| *b == byte)? as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            result.push((bits >> n) as u8);
        }
    }
    if n >= 6 || bits & ((1 << n) - 1) != 0 {
        return None;
    }
    Some(result)
}
fn encode(bytes: &[u8]) -> String {
    let mut result = String::new();
    let (mut bits, mut n) = (0u32, 0);
    for byte in bytes {
        bits = (bits << 8) | u32::from(*byte);
        n += 8;
        while n >= 6 {
            n -= 6;
            result.push(B64[((bits >> n) & 63) as usize] as char);
        }
    }
    if n > 0 {
        result.push(B64[((bits << (6 - n)) & 63) as usize] as char);
    }
    result
}
fn token(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start();
    if line.is_empty() {
        return None;
    }
    let (mut quote, mut escape) = (false, false);
    for (i, c) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if c == '\\' {
            escape = true;
        } else if c == '"' {
            quote = !quote;
        } else if c.is_whitespace() && !quote {
            return Some((&line[..i], &line[i..]));
        }
    }
    if quote || escape {
        None
    } else {
        Some((line, ""))
    }
}
pub(super) fn parse(line: &str) -> Option<PublicKey> {
    if line.contains(['\n', '\r', '\0']) || line.trim_start().starts_with('#') {
        return None;
    }
    let (first, rest) = token(line)?;
    let (kind, rest) =
        if first.starts_with("ssh-") || first.starts_with("ecdsa-") || first.starts_with("sk-") {
            (first, rest)
        } else {
            token(rest)?
        };
    if !kind.starts_with("ssh-") && !kind.starts_with("ecdsa-") && !kind.starts_with("sk-") {
        return None;
    }
    let (data, rest) = token(rest)?;
    let bytes = decode(data)?;
    let unpadded = data.trim_end_matches('=');
    let padding = data.len() - unpadded.len();
    if encode(&bytes) != unpadded || (padding != 0 && (padding > 2 || data.len() % 4 != 0)) {
        return None;
    }
    let size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    if bytes.get(4..4 + size)? != kind.as_bytes() || bytes.len() <= 4 + size {
        return None;
    }
    if kind == "ssh-ed25519" && (bytes.len() != 51 || bytes.get(15..19)? != 32u32.to_be_bytes()) {
        return None;
    }
    Some(PublicKey {
        kind: kind.into(),
        data: data.into(),
        comment: rest.trim().into(),
        fingerprint: format!("SHA256:{}", encode(&Sha256::digest(bytes))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 解析选项注释与坏行() {
        let line =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let a = parse(line).unwrap();
        let b = parse(&format!(
            "command=\"echo ssh-rsa fake\",from=\"host\" {line} pierlink-a comment"
        ))
        .unwrap();
        assert_eq!(a.data, b.data);
        assert_eq!(b.comment, "pierlink-a comment");
        for bad in ["bad", "ssh-ed25519 !!!!", "ssh-rsa AAAA", "# comment"] {
            assert!(parse(bad).is_none());
        }
    }
}
