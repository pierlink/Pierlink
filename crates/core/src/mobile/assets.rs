//! 只读静态资源，先解析编码和规范路径，再确认目标仍在资源根目录。
use super::{
    error,
    http::{HttpResponse, bytes_response},
    *,
};
use ::http::{HeaderValue, StatusCode, header};
use std::path::{Component, Path};

pub(super) async fn serve(root: PathBuf, path: &str) -> Result<HttpResponse, AppError> {
    let relative = relative_path(path)?;
    let (bytes, path) = tokio::task::spawn_blocking(move || read(&root, &relative))
        .await
        .map_err(|_| error("mobile.assets", "网页资源读取任务失败"))??;
    let mut response = bytes_response(StatusCode::OK, bytes);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&path)),
    );
    let cache = if path.file_name().is_some_and(|n| n == "index.html") {
        "no-store"
    } else if hashed(&path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    Ok(response)
}

fn relative_path(path: &str) -> Result<PathBuf, AppError> {
    let invalid = || error("mobile.invalid_path", "网页资源路径不合法");
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let pair = bytes.get(i + 1..i + 3).ok_or_else(invalid)?;
            let hex = std::str::from_utf8(pair).map_err(|_| invalid())?;
            decoded.push(u8::from_str_radix(hex, 16).map_err(|_| invalid())?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = std::str::from_utf8(&decoded).map_err(|_| invalid())?;
    let relative = decoded.strip_prefix('/').ok_or_else(invalid)?;
    if relative.starts_with('/')
        || relative.contains(['\\', ':', '\0', '%'])
        || relative.split('/').any(|p| p == ".." || p == ".")
    {
        return Err(invalid());
    }
    let path = PathBuf::from(relative);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(invalid());
    }
    Ok(path)
}

fn read(root: &Path, relative: &Path) -> Result<(Vec<u8>, PathBuf), AppError> {
    let requested = if relative.as_os_str().is_empty() {
        root.join("index.html")
    } else {
        root.join(relative)
    };
    let path = match std::fs::canonicalize(&requested) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 缺失文件的父目录也必须留在资源根内，不能通过逃逸符号链接触发回退。
            let mut parent = requested.parent();
            while let Some(p) = parent {
                match std::fs::canonicalize(p) {
                    Ok(resolved) => {
                        if !resolved.starts_with(root) {
                            return Err(error("mobile.invalid_path", "资源路径越出了网页目录"));
                        }
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => parent = p.parent(),
                    Err(_) => return Err(error("mobile.assets", "无法检查网页资源目录")),
                }
            }
            std::fs::canonicalize(root.join("index.html"))
                .map_err(|_| error("mobile.not_found", "网页首页不存在"))?
        }
        Err(_) => return Err(error("mobile.assets", "无法读取网页资源路径")),
    };
    if !path.starts_with(root) {
        return Err(error("mobile.invalid_path", "资源路径越出了网页目录"));
    }
    let metadata =
        std::fs::metadata(&path).map_err(|_| error("mobile.assets", "无法读取网页资源信息"))?;
    if !metadata.is_file() {
        return Err(error("mobile.not_found", "网页资源不是普通文件"));
    }
    // 资源为打包时生成的小文件，防止误配置目录导致任意大文件占满内存。
    const MAX_ASSET: u64 = 16 * 1024 * 1024;
    if metadata.len() > MAX_ASSET {
        return Err(error("mobile.assets", "网页资源超过十六 MiB 上限"));
    }
    use std::io::Read;
    let file =
        std::fs::File::open(&path).map_err(|_| error("mobile.assets", "无法打开网页资源"))?;
    let mut bytes = Vec::new();
    file.take(MAX_ASSET + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error("mobile.assets", "无法读取网页资源"))?;
    if bytes.len() as u64 > MAX_ASSET {
        return Err(error("mobile.assets", "网页资源超过十六 MiB 上限"));
    }
    Ok((bytes, path))
}

fn hashed(path: &Path) -> bool {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    stem.rsplit_once(['-', '.']).is_some_and(|(_, hash)| {
        (8..=64).contains(&hash.len())
            && hash.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}
fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "webmanifest" => "application/manifest+json",
        _ => "application/octet-stream",
    }
}
