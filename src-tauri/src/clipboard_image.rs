//! 读剪贴板里的图片，统一成 PNG：剪贴板里有现成的“PNG”格式（浏览器、截图工具常带）就直接用，
//! 否则把 Windows 位图（CF_DIBV5、CF_DIB）转成 PNG。位图只支持 24 位和 32 位（截图、复制图片都是这两种）。
//!
//! 位图的格式见微软文档 BITMAPINFOHEADER / BITMAPV5HEADER：头的前 40 字节各版本相同，
//! 后面依次是（可能有的）颜色掩码、调色板、像素行；每行补齐到 4 字节，高度为正时最下面一行在前。
use pierlink_core::error::AppError;

/// 宽、高的上限：超过就当成太大，不去分配内存
const MAX_SIDE: u32 = 20_000;
/// 像素总数上限（约 1 亿像素）
const MAX_PIXELS: u64 = 100_000_000;
/// PNG 文件开头的 8 个字节
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

// 位图头里各字段的偏移
const AT_WIDTH: usize = 4;
const AT_HEIGHT: usize = 8;
const AT_BIT_COUNT: usize = 14;
const AT_COMPRESSION: usize = 16;
const AT_CLR_USED: usize = 32;
/// 红、绿、蓝、透明度掩码：V2 及以上的头里在这个位置；40 字节的头加 BI_BITFIELDS 时，三个掩码紧跟在头后面，也正好从这里开始
const AT_MASKS: usize = 40;
/// 各版本位图头的长度：BITMAPINFOHEADER、V2、V3、V4、V5
const HEADER_LENGTHS: [usize; 5] = [40, 52, 56, 108, 124];

/// 压缩方式：不压缩（固定的 BGR / BGRX 排列）
const BI_RGB: u32 = 0;
/// 压缩方式：按颜色掩码取各分量
const BI_BITFIELDS: u32 = 3;

/// 剪贴板里的图片。
///
/// :return: 有图片时返回 PNG 字节；没有图片返回 None；剪贴板被别的程序占着、格式不支持时返回错误
pub fn read_png() -> Result<Option<Vec<u8>>, AppError> {
    platform::read_png()
}

fn unsupported() -> AppError {
    AppError::new("paste.image_unsupported")
}

/// 现成的 PNG 数据：校验开头，并截到 IEND 块为止（剪贴板的内存块常比内容大，后面是填充）。
///
/// :param bytes: 剪贴板里“PNG”格式的原始字节
/// :return: 截好的 PNG；不是 PNG 或块不完整时返回 None
#[cfg_attr(not(windows), allow(dead_code))]
fn trim_png(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.starts_with(PNG_SIGNATURE) {
        return None;
    }
    // 每个块：4 字节长度（大端）+ 4 字节类型 + 数据 + 4 字节校验
    let mut at = PNG_SIGNATURE.len();
    loop {
        let len =
            usize::try_from(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?)).ok()?;
        let kind = bytes.get(at + 4..at + 8)?;
        let next = at.checked_add(12)?.checked_add(len)?;
        if next > bytes.len() {
            return None;
        }
        if kind == b"IEND" {
            return Some(bytes[..next].to_vec());
        }
        at = next;
    }
}

/// 小端读 2 字节
fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

/// 小端读 4 字节
fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// 从位图头里读出的、转换要用的信息
#[derive(Debug, PartialEq)]
struct Layout {
    width: usize,
    height: usize,
    /// 第一行像素是不是最上面一行（高度为负）
    top_down: bool,
    /// 每个像素几个字节：3 或 4
    bytes_per_pixel: usize,
    /// 红、绿、蓝、透明度的掩码；透明度为 0 表示没有透明度
    masks: [u32; 4],
    /// 像素数据从第几个字节开始
    pixels_at: usize,
    /// 每行占几个字节（含补齐）
    stride: usize,
}

/// 读位图头。
///
/// :param dib: CF_DIB / CF_DIBV5 的数据
/// :return: 转换要用的信息；不是 24、32 位位图或数据不完整返回 paste.image_unsupported，太大返回 paste.image_too_large
fn read_layout(dib: &[u8]) -> Result<Layout, AppError> {
    let header = read_u32(dib, 0)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(unsupported)?;
    if !HEADER_LENGTHS.contains(&header) || header > dib.len() {
        return Err(unsupported());
    }
    let field = |at: usize| read_u32(dib, at).ok_or_else(unsupported);
    let width = field(AT_WIDTH)? as i32;
    let height = field(AT_HEIGHT)? as i32;
    let bits = read_u16(dib, AT_BIT_COUNT).ok_or_else(unsupported)?;
    let compression = field(AT_COMPRESSION)?;
    if width <= 0 || height == 0 {
        return Err(unsupported());
    }
    // 高度为负：第一行是最上面一行
    let top_down = height < 0;
    let (w, h) = (width.unsigned_abs(), height.unsigned_abs());
    if w > MAX_SIDE || h > MAX_SIDE || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(AppError::new("paste.image_too_large"));
    }
    // 掩码：不压缩时是固定排列（32 位的最高字节可能是透明度，见 rgba_pixels）；BI_BITFIELDS 时从文件里读
    let (masks, masks_after_header) = match (compression, bits) {
        (BI_RGB, 24) => ([0xff_0000, 0xff00, 0xff, 0], 0),
        (BI_RGB, 32) => ([0xff_0000, 0xff00, 0xff, 0xff00_0000], 0),
        (BI_BITFIELDS, 32) => {
            let alpha = if header >= 56 {
                field(AT_MASKS + 12)?
            } else {
                0
            };
            let rgb = [field(AT_MASKS)?, field(AT_MASKS + 4)?, field(AT_MASKS + 8)?];
            (
                [rgb[0], rgb[1], rgb[2], alpha],
                if header == 40 { 12 } else { 0 },
            )
        }
        _ => return Err(unsupported()),
    };
    if masks[..3].contains(&0) {
        return Err(unsupported());
    }
    // 24、32 位位图也可能带一张“优化显示用”的调色板（每项 4 字节），跳过
    let palette = usize::try_from(field(AT_CLR_USED)?)
        .ok()
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(unsupported)?;
    let pixels_at = header
        .checked_add(masks_after_header)
        .and_then(|n| n.checked_add(palette))
        .ok_or_else(unsupported)?;
    let bytes_per_pixel = usize::from(bits / 8);
    let (width, height) = (w as usize, h as usize);
    // 每行按 4 字节补齐
    let stride = (width * bytes_per_pixel).div_ceil(4) * 4;
    let end = stride
        .checked_mul(height)
        .and_then(|n| n.checked_add(pixels_at))
        .ok_or_else(unsupported)?;
    if end > dib.len() {
        return Err(unsupported());
    }
    Ok(Layout {
        width,
        height,
        top_down,
        bytes_per_pixel,
        masks,
        pixels_at,
        stride,
    })
}

/// 按掩码取出一个颜色分量，换算到 0–255（掩码位数不是 8 时按比例缩放）
fn scale(pixel: u32, mask: u32) -> u8 {
    let max = u64::from(mask >> mask.trailing_zeros());
    let value = u64::from((pixel & mask) >> mask.trailing_zeros());
    (value * 255 / max) as u8
}

/// 按从上到下的顺序取出所有像素，排成 RGBA。
///
/// 很多程序放进剪贴板的 32 位位图其实没用透明度、最高字节全是 0：这种情况当成不透明，否则整张图是透明的。
///
/// :param dib: 位图数据
/// :param layout: read_layout 读出的信息（已确认数据够长）
/// :return: 每像素 4 字节的 RGBA
fn rgba_pixels(dib: &[u8], layout: &Layout) -> Vec<u8> {
    let mut out = Vec::with_capacity(layout.width * layout.height * 4);
    let mut has_alpha = false;
    for y in 0..layout.height {
        let stored = if layout.top_down {
            y
        } else {
            layout.height - 1 - y
        };
        let row = &dib[layout.pixels_at + stored * layout.stride..]
            [..layout.width * layout.bytes_per_pixel];
        for px in row.chunks_exact(layout.bytes_per_pixel) {
            // 小端：第一个字节是最低位
            let pixel = px
                .iter()
                .rev()
                .fold(0u32, |acc, &b| acc << 8 | u32::from(b));
            let [r, g, b, a] = layout.masks;
            let alpha = if a == 0 { 255 } else { scale(pixel, a) };
            has_alpha |= a != 0 && alpha != 0;
            out.extend([scale(pixel, r), scale(pixel, g), scale(pixel, b), alpha]);
        }
    }
    if layout.masks[3] != 0 && !has_alpha {
        out.iter_mut().skip(3).step_by(4).for_each(|a| *a = 255);
    }
    out
}

/// 把 RGBA 像素编码成 PNG
fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, AppError> {
    let failed = |e: png::EncodingError| {
        AppError::new("paste.image_unsupported").with("reason", e.to_string())
    };
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(failed)?;
    writer.write_image_data(rgba).map_err(failed)?;
    writer.finish().map_err(failed)?;
    Ok(out)
}

/// 把 Windows 位图转成 PNG。
///
/// :param dib: CF_DIB / CF_DIBV5 的数据（位图头开头，后面是掩码、调色板和像素）
/// :return: PNG 字节；格式不支持返回 paste.image_unsupported，太大返回 paste.image_too_large
#[cfg_attr(not(windows), allow(dead_code))]
fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, AppError> {
    let layout = read_layout(dib)?;
    encode_png(layout.width, layout.height, &rgba_pixels(dib, &layout))
}

#[cfg(windows)]
mod platform {
    use super::{dib_to_png, trim_png};
    use pierlink_core::error::AppError;
    use std::time::Duration;
    use windows_sys::Win32::System::DataExchange as dx;
    use windows_sys::Win32::System::Memory as mem;

    /// 标准剪贴板格式编号：CF_DIB、CF_DIBV5（V5 带透明度，优先）
    const BITMAP_FORMATS: [u32; 2] = [17, 8];
    /// 剪贴板被别的程序（输入法、剪贴板历史等）短暂占用时的等待：依次等这么多毫秒再试
    const OPEN_WAITS_MS: [u64; 6] = [5, 15, 30, 60, 100, 150];

    /// 离开作用域时关闭剪贴板（只在打开成功后创建）
    struct CloseOnDrop;

    impl Drop for CloseOnDrop {
        fn drop(&mut self) {
            // SAFETY: 本线程已打开剪贴板
            unsafe { dx::CloseClipboard() };
        }
    }

    /// 打开剪贴板，被占用时按 OPEN_WAITS_MS 等待后重试
    fn open() -> Result<CloseOnDrop, AppError> {
        let mut waits = OPEN_WAITS_MS.iter();
        loop {
            // SAFETY: 不关联窗口，只读
            if unsafe { dx::OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(CloseOnDrop);
            }
            match waits.next() {
                Some(ms) => std::thread::sleep(Duration::from_millis(*ms)),
                None => return Err(AppError::new("clipboard.busy")),
            }
        }
    }

    /// 复制出一种格式的数据；剪贴板里没有这种格式返回 None
    fn copy_format(format: u32) -> Result<Option<Vec<u8>>, AppError> {
        // SAFETY: 剪贴板已打开；句柄是剪贴板持有的全局内存，锁住后按 GlobalSize 复制，再解锁
        unsafe {
            let handle = dx::GetClipboardData(format);
            if handle.is_null() {
                return Ok(None);
            }
            let start = mem::GlobalLock(handle).cast::<u8>().cast_const();
            if start.is_null() {
                return Err(AppError::new("clipboard.read_failed"));
            }
            let copied = std::slice::from_raw_parts(start, mem::GlobalSize(handle)).to_vec();
            mem::GlobalUnlock(handle);
            Ok(Some(copied))
        }
    }

    /// 注册过的“PNG”格式编号；拿不到返回 None
    fn png_format() -> Option<u32> {
        let name: Vec<u16> = "PNG\0".encode_utf16().collect();
        // SAFETY: name 以 0 结尾
        let id = unsafe { dx::RegisterClipboardFormatW(name.as_ptr()) };
        (id != 0).then_some(id)
    }

    pub(super) fn read_png() -> Result<Option<Vec<u8>>, AppError> {
        let _close = open()?;
        if let Some(format) = png_format()
            && let Some(bytes) = copy_format(format)?
            && let Some(png) = trim_png(&bytes)
        {
            return Ok(Some(png));
        }
        for format in BITMAP_FORMATS {
            if let Some(dib) = copy_format(format)? {
                return dib_to_png(&dib).map(Some);
            }
        }
        Ok(None)
    }
}

#[cfg(not(windows))]
mod platform {
    use pierlink_core::error::AppError;

    /// 只有 Windows 版读剪贴板图片
    pub(super) fn read_png() -> Result<Option<Vec<u8>>, AppError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼一个位图：header 是头的长度；masks 有值时按头的长度放进头里或跟在头后面；
    /// rows 按存放顺序给出每行的像素字节（不含行尾补齐）
    fn dib(
        header: u32,
        width: i32,
        height: i32,
        bits: u16,
        compression: u32,
        masks: Option<[u32; 4]>,
        rows: &[Vec<u8>],
    ) -> Vec<u8> {
        let mut out = vec![0u8; header as usize];
        out[0..4].copy_from_slice(&header.to_le_bytes());
        out[4..8].copy_from_slice(&width.to_le_bytes());
        out[8..12].copy_from_slice(&height.to_le_bytes());
        out[12..14].copy_from_slice(&1u16.to_le_bytes());
        out[14..16].copy_from_slice(&bits.to_le_bytes());
        out[16..20].copy_from_slice(&compression.to_le_bytes());
        if let Some(m) = masks {
            if header >= 56 {
                for (i, v) in m.iter().enumerate() {
                    out[40 + i * 4..44 + i * 4].copy_from_slice(&v.to_le_bytes());
                }
            } else {
                for v in &m[..3] {
                    out.extend(v.to_le_bytes());
                }
            }
        }
        for row in rows {
            let mut r = row.clone();
            while r.len() % 4 != 0 {
                r.push(0);
            }
            out.extend(r);
        }
        out
    }

    /// 解出 PNG 的 RGBA 像素
    fn decode(png_bytes: &[u8]) -> (u32, u32, Vec<u8>) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    #[test]
    fn 自下而上的24位位图转成上下正确的png() {
        // 两行两列：存放顺序先是最下面一行（蓝、白），再是最上面一行（红、绿）；每行 6 字节补齐到 8
        let bottom = vec![255, 0, 0, 255, 255, 255];
        let top = vec![0, 0, 255, 0, 255, 0];
        let (w, h, px) = decode(&dib_to_png(&dib(40, 2, 2, 24, 0, None, &[bottom, top])).unwrap());
        assert_eq!((w, h), (2, 2));
        assert_eq!(&px[0..8], &[255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(&px[8..16], &[0, 0, 255, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn 自上而下的32位位图透明度全为0时当成不透明() {
        // 高度为负表示自上而下；BGRA，透明度都是 0
        let row = vec![0, 0, 255, 0, 0, 255, 0, 0];
        let (_, _, px) = decode(&dib_to_png(&dib(40, 2, -1, 32, 0, None, &[row])).unwrap());
        assert_eq!(px, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn v5头带透明度掩码时保留透明度() {
        let masks = [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000];
        let row = vec![0, 0, 255, 128, 0, 0, 0, 0];
        let (_, _, px) = decode(&dib_to_png(&dib(124, 2, -1, 32, 3, Some(masks), &[row])).unwrap());
        assert_eq!(px, vec![255, 0, 0, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn bitfields掩码跟在40字节头后面() {
        // 32 位 BITFIELDS，掩码按 R、G、B 给出，跟在 40 字节的头后面
        let masks = [0x0000_00ff, 0x0000_ff00, 0x00ff_0000, 0];
        let row = vec![10, 20, 30, 0];
        let (_, _, px) = decode(&dib_to_png(&dib(40, 1, -1, 32, 3, Some(masks), &[row])).unwrap());
        assert_eq!(px, vec![10, 20, 30, 255]);
    }

    #[test]
    fn 十位色深的掩码按比例换算() {
        // 2:10:10:10 排列：蓝在最低 10 位，绿、红依次往上，最高 2 位是透明度
        let masks = [0x3ff0_0000, 0x000f_fc00, 0x0000_03ff, 0xc000_0000];
        let pixel: u32 = 0xc000_0000 | (1023 << 20) | (512 << 10);
        let (_, _, px) = decode(
            &dib_to_png(&dib(
                124,
                1,
                -1,
                32,
                3,
                Some(masks),
                &[pixel.to_le_bytes().to_vec()],
            ))
            .unwrap(),
        );
        assert_eq!(px, vec![255, 127, 0, 255]);
    }

    #[test]
    fn 调色板会被跳过() {
        // biClrUsed = 2：头后面先有 8 字节调色板，再是像素
        let mut data = dib(40, 1, 1, 24, 0, None, &[]);
        data[32..36].copy_from_slice(&2u32.to_le_bytes());
        data.extend([9u8; 8]);
        data.extend([3, 2, 1, 0]);
        let (_, _, px) = decode(&dib_to_png(&data).unwrap());
        assert_eq!(px, vec![1, 2, 3, 255]);
    }

    #[test]
    fn 不支持的格式和坏数据报错不崩溃() {
        let code = |data: &[u8]| dib_to_png(data).unwrap_err().code;
        // 8 位调色板位图、16 位位图不支持
        assert_eq!(
            code(&dib(40, 1, 1, 8, 0, None, &[vec![0]])),
            "paste.image_unsupported"
        );
        assert_eq!(
            code(&dib(40, 1, 1, 16, 0, None, &[vec![0, 0]])),
            "paste.image_unsupported"
        );
        // 太短、头长度不认识
        assert_eq!(code(&[1, 2, 3]), "paste.image_unsupported");
        assert_eq!(
            code(&dib(44, 1, 1, 24, 0, None, &[vec![1, 2, 3]])),
            "paste.image_unsupported"
        );
        // 头说有 100 行，数据不够
        let mut short = dib(40, 2, 100, 24, 0, None, &[vec![0; 6]]);
        short.truncate(50);
        assert_eq!(code(&short), "paste.image_unsupported");
        // 调色板长度大得离谱
        let mut palette = dib(40, 1, 1, 24, 0, None, &[vec![1, 2, 3]]);
        palette[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(code(&palette), "paste.image_unsupported");
        // 掩码为 0
        assert_eq!(
            code(&dib(
                40,
                1,
                -1,
                32,
                3,
                Some([0, 0xff00, 0xff, 0]),
                &[vec![0; 4]]
            )),
            "paste.image_unsupported"
        );
        // 宽或高为 0、过大
        assert_eq!(
            code(&dib(40, 0, 1, 24, 0, None, &[])),
            "paste.image_unsupported"
        );
        assert_eq!(
            code(&dib(40, 30_000, 1, 24, 0, None, &[])),
            "paste.image_too_large"
        );
        assert_eq!(
            code(&dib(40, 1, i32::MIN, 24, 0, None, &[])),
            "paste.image_too_large"
        );
    }

    #[test]
    fn 现成的png截到iend为止() {
        let png = dib_to_png(&dib(40, 1, 1, 24, 0, None, &[vec![1, 2, 3]])).unwrap();
        let mut padded = png.clone();
        padded.extend([0u8; 37]);
        assert_eq!(trim_png(&padded).unwrap(), png);
        assert!(trim_png(b"not a png").is_none());
        // 截断在块中间
        assert!(trim_png(&png[..png.len() - 5]).is_none());
    }
}
