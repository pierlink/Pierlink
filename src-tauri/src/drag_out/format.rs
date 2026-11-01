//! 系统拖放数据的字节格式。和平台无关，放在这里方便在 Linux 上测试；只有 Windows 的拖放用到它。
#![cfg_attr(not(windows), allow(dead_code))]

/// DROPFILES 结构的字节数：pFiles(4) + pt(8) + fNC(4) + fWide(4)
pub const DROPFILES_SIZE: usize = 20;

/// CF_HDROP 的内容：DROPFILES 头，后面紧跟宽字符路径，每个以 0 结尾，最后再多一个 0。
///
/// :param paths: 每个路径的 UTF-16 编码（不含结尾的 0）
/// :return: 放进 HGLOBAL 的字节（小端）
pub fn hdrop_bytes(paths: &[Vec<u16>]) -> Vec<u8> {
    let units: usize = paths.iter().map(|p| p.len() + 1).sum::<usize>() + 1;
    let mut out = Vec::with_capacity(DROPFILES_SIZE + units * 2);
    // pFiles：路径列表紧跟在头后面
    out.extend_from_slice(&(DROPFILES_SIZE as u32).to_le_bytes());
    // pt（放下的位置，不用）
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    // fNC：pt 不是非客户区坐标
    out.extend_from_slice(&0u32.to_le_bytes());
    // fWide：路径是宽字符
    out.extend_from_slice(&1u32.to_le_bytes());
    for path in paths {
        for unit in path {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    // 列表结尾再多一个 0（一项也没有时也要成对的两个 0）
    out.extend_from_slice(&0u16.to_le_bytes());
    if paths.is_empty() {
        out.extend_from_slice(&0u16.to_le_bytes());
    }
    out
}

/// “Preferred DropEffect” 格式的内容：一个 DWORD（小端）
///
/// :param effect: DROPEFFECT 的值（移动是 2）
/// :return: 放进 HGLOBAL 的 4 个字节
pub fn effect_bytes(effect: u32) -> Vec<u8> {
    effect.to_le_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    /// 从字节里读回第 i 个 u32（小端）
    fn u32_at(bytes: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
    }

    /// 从头后面读回宽字符路径列表
    fn read_paths(bytes: &[u8]) -> Vec<String> {
        let (pairs, _) = bytes[DROPFILES_SIZE..].as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
        let mut out = Vec::new();
        let mut start = 0;
        while start < units.len() && units[start] != 0 {
            let end = start + units[start..].iter().position(|&u| u == 0).unwrap();
            out.push(String::from_utf16(&units[start..end]).unwrap());
            start = end + 1;
        }
        out
    }

    #[test]
    fn 头部指明路径从第20字节开始且是宽字符() {
        let bytes = hdrop_bytes(&[wide("C:\\a.txt")]);
        assert_eq!(u32_at(&bytes, 0), 20);
        assert_eq!(u32_at(&bytes, 4), 0);
        assert_eq!(u32_at(&bytes, 8), 0);
        assert_eq!(u32_at(&bytes, 12), 0);
        assert_eq!(u32_at(&bytes, 16), 1);
    }

    #[test]
    fn 每个路径以0结尾且列表多一个0() {
        let bytes = hdrop_bytes(&[wide("C:\\a"), wide("C:\\b")]);
        // 头 20 字节 + ("C:\a" 4 个字符 + 0) × 2 + 结尾 0，每个字符 2 字节
        assert_eq!(bytes.len(), 20 + (5 + 5 + 1) * 2);
        assert_eq!(&bytes[bytes.len() - 4..], &[0, 0, 0, 0]);
        assert_eq!(read_paths(&bytes), vec!["C:\\a", "C:\\b"]);
    }

    #[test]
    fn 中文和长路径原样编码() {
        let long = format!("D:\\数据\\{}\\报告.pdf", "子目录".repeat(40));
        let bytes = hdrop_bytes(&[wide(&long), wide("C:\\x y\\😀.txt")]);
        assert_eq!(read_paths(&bytes), vec![long, "C:\\x y\\😀.txt".to_owned()]);
    }

    #[test]
    fn 没有路径时也以两个0结尾() {
        let bytes = hdrop_bytes(&[]);
        assert_eq!(bytes.len(), 24);
        assert_eq!(&bytes[20..], &[0, 0, 0, 0]);
        assert!(read_paths(&bytes).is_empty());
    }

    #[test]
    fn 首选效果是一个小端的双字() {
        assert_eq!(effect_bytes(2), vec![2, 0, 0, 0]);
        assert_eq!(effect_bytes(0x8000_0001), vec![1, 0, 0, 0x80]);
    }
}
