//! 仅用于测试的 RFC 6238 SHA-1/30 秒验证码，不进入引擎或产品代码。
//! 无额外依赖；下方使用 RFC 6238 的全部 SHA-1 测试向量核对。

pub fn code(base32: &str, seconds: u64, digits: u32) -> String {
    let mut key = Vec::new();
    let mut bits = 0u32;
    let mut count = 0;
    for byte in base32
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=')
    {
        let byte = byte.to_ascii_uppercase();
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => panic!("TOTP 密钥不是有效 base32"),
        };
        bits = (bits << 5) | u32::from(value);
        count += 5;
        if count >= 8 {
            count -= 8;
            key.push((bits >> count) as u8);
        }
    }
    assert!(!key.is_empty(), "TOTP 密钥不能为空");
    if key.len() > 64 {
        key = sha1(&key).to_vec();
    }
    key.resize(64, 0);
    let mut inner: Vec<u8> = key.iter().map(|value| value ^ 0x36).collect();
    inner.extend_from_slice(&(seconds / 30).to_be_bytes());
    let mut outer: Vec<u8> = key.iter().map(|value| value ^ 0x5c).collect();
    outer.extend_from_slice(&sha1(&inner));
    let digest = sha1(&outer);
    let offset = usize::from(digest[19] & 0xf);
    let value = u32::from_be_bytes(digest[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
    format!(
        "{:0width$}",
        value % 10u32.pow(digits),
        width = digits as usize
    )
}

fn sha1(input: &[u8]) -> [u8; 20] {
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((input.len() as u64) * 8).to_be_bytes());
    let mut state = [
        0x67452301u32,
        0xefcdab89,
        0x98badcfe,
        0x10325476,
        0xc3d2e1f0,
    ];
    for block in message.as_chunks::<64>().0 {
        let mut words = [0u32; 80];
        for (word, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.into_iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999u32),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (state, value) in state.iter_mut().zip([a, b, c, d, e]) {
            *state = state.wrapping_add(value);
        }
    }
    let mut output = [0; 20];
    for (bytes, word) in output.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    output
}

#[test]
fn rfc6238_sha1_vectors() {
    let secret = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
    for (time, expected) in [
        (59, "94287082"),
        (1111111109, "07081804"),
        (1111111111, "14050471"),
        (1234567890, "89005924"),
        (2000000000, "69279037"),
        (20000000000, "65353130"),
    ] {
        assert_eq!(code(secret, time, 8), expected);
    }
    assert_eq!(code(secret, 59, 6), "287082");
}
