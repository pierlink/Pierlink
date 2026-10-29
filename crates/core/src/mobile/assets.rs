//! 只读静态资源，先解析编码和规范路径，再确认目标仍在资源根目录。
use super::{
    error,
    http::{HttpResponse, bytes_response},
    *,
};
use ::http::{HeaderValue, StatusCode, header};
use std::path::{Component, Path};

pub(super) async fn serve(
    root: PathBuf,
    path: &str,
    accept_encoding: Option<&str>,
) -> Result<HttpResponse, AppError> {
    let relative = relative_path(path)?;
    let accepts_gzip = accepts_gzip(accept_encoding);
    let asset = tokio::task::spawn_blocking(move || read(&root, &relative, accepts_gzip))
        .await
        .map_err(|_| error("mobile.assets", "网页资源读取任务失败"))??;
    let mut response = bytes_response(StatusCode::OK, asset.bytes);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&asset.path)),
    );
    let cache = if asset.path.file_name().is_some_and(|n| n == "index.html") {
        "no-store"
    } else if hashed(&asset.path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if asset.has_gzip {
        headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    if asset.compressed {
        headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    Ok(response)
}

fn accepts_gzip(value: Option<&str>) -> bool {
    let mut accepted = false;
    for item in value.unwrap_or_default().split(',') {
        let mut parts = item.split(';');
        if !parts
            .next()
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("gzip"))
        {
            continue;
        }
        for parameter in parts {
            let Some((name, value)) = parameter.trim().split_once('=') else {
                return false;
            };
            if !name.trim().eq_ignore_ascii_case("q")
                || !value
                    .trim()
                    .parse::<f32>()
                    .is_ok_and(|q| q > 0.0 && q <= 1.0)
            {
                return false;
            }
        }
        accepted = true;
    }
    accepted
}

struct Asset {
    bytes: Vec<u8>,
    path: PathBuf,
    has_gzip: bool,
    compressed: bool,
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

fn read(root: &Path, relative: &Path, accepts_gzip: bool) -> Result<Asset, AppError> {
    let mut requested = if relative.as_os_str().is_empty() {
        root.join("index.html")
    } else {
        root.join(relative)
    };
    let path = match resolve(root, &requested)? {
        Some(path) => path,
        None => {
            requested = root.join("index.html");
            resolve(root, &requested)?.ok_or_else(|| error("mobile.not_found", "网页首页不存在"))?
        }
    };
    // 原文件须存在且安全，才能选择它旁边的压缩文件；回退首页时也使用首页的压缩文件。
    check_file(&path)?;
    let mut gzip_name = requested.as_os_str().to_os_string();
    gzip_name.push(".gz");
    let gzip_path = resolve(root, Path::new(&gzip_name))?;
    if let Some(gzip) = &gzip_path {
        check_file(gzip)?;
    }
    let selected = gzip_path
        .as_deref()
        .filter(|_| accepts_gzip)
        .unwrap_or(&path);
    Ok(Asset {
        bytes: read_file(selected)?,
        path,
        has_gzip: gzip_path.is_some(),
        compressed: accepts_gzip && gzip_path.is_some(),
    })
}

fn resolve(root: &Path, requested: &Path) -> Result<Option<PathBuf>, AppError> {
    let path = match std::fs::canonicalize(requested) {
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
            return Ok(None);
        }
        Err(_) => return Err(error("mobile.assets", "无法读取网页资源路径")),
    };
    if !path.starts_with(root) {
        return Err(error("mobile.invalid_path", "资源路径越出了网页目录"));
    }
    Ok(Some(path))
}

const MAX_ASSET: u64 = 16 * 1024 * 1024;

fn check_file(path: &Path) -> Result<(), AppError> {
    let metadata =
        std::fs::metadata(path).map_err(|_| error("mobile.assets", "无法读取网页资源信息"))?;
    if !metadata.is_file() {
        return Err(error("mobile.not_found", "网页资源不是普通文件"));
    }
    // 资源为打包时生成的小文件，防止误配置目录导致任意大文件占满内存。
    if metadata.len() > MAX_ASSET {
        return Err(error("mobile.assets", "网页资源超过十六 MiB 上限"));
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<Vec<u8>, AppError> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| error("mobile.assets", "无法打开网页资源"))?;
    let mut bytes = Vec::new();
    file.take(MAX_ASSET + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| error("mobile.assets", "无法读取网页资源"))?;
    if bytes.len() as u64 > MAX_ASSET {
        return Err(error("mobile.assets", "网页资源超过十六 MiB 上限"));
    }
    Ok(bytes)
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

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use std::io::{Read, Write};

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[tokio::test]
    async fn 压缩协商保持原文件类型缓存并声明变化头() {
        let root = crate::test_support::tempdir();
        let plain = b"console.log(1)";
        let compressed = gzip(plain);
        std::fs::write(root.path().join("app-a1b2c3d4.js"), plain).unwrap();
        std::fs::write(root.path().join("app-a1b2c3d4.js.gz"), &compressed).unwrap();
        for (accept, use_gzip) in [
            (None, false),
            (Some(""), false),
            (Some("br"), false),
            (Some("gzip"), true),
            (Some("br, gzip;q=0.5"), true),
            (Some(" GZip ; q = 1.0 "), true),
            (Some("gzip;q=0"), false),
            (Some("gzip;q=0.000, br"), false),
            (Some("xgzip"), false),
            (Some("*;q=1, gzip;q=0"), false),
            (Some("gzip;q=invalid"), false),
            (Some("gzip;q=0, gzip"), false),
        ] {
            let response = serve(root.path().into(), "/app-a1b2c3d4.js", accept)
                .await
                .unwrap();
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/javascript; charset=utf-8"
            );
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "public, max-age=31536000, immutable"
            );
            assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
            assert_eq!(
                response.headers().contains_key(header::CONTENT_ENCODING),
                use_gzip,
                "{accept:?}"
            );
            if use_gzip {
                assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
            }
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                body.as_ref(),
                if use_gzip { &compressed[..] } else { plain },
                "{accept:?}"
            );
            if use_gzip {
                let mut decoded = Vec::new();
                flate2::read::GzDecoder::new(body.as_ref())
                    .read_to_end(&mut decoded)
                    .unwrap();
                assert_eq!(decoded, plain);
            }
        }
    }

    #[tokio::test]
    async fn 首页压缩仍不缓存且缺少压缩文件时返回原文件() {
        let root = crate::test_support::tempdir();
        std::fs::write(root.path().join("index.html"), "首页").unwrap();
        std::fs::write(root.path().join("index.html.gz"), gzip("首页".as_bytes())).unwrap();
        std::fs::write(root.path().join("style.css"), "body{}").unwrap();
        // 单页路由和缺失资源仍按原有规则回退首页，不能借孤立的压缩文件返回内容。
        std::fs::write(root.path().join("missing.js.gz"), "孤立资源").unwrap();
        for path in ["/", "/index.html", "/route/page", "/missing.js"] {
            for accept in [None, Some("gzip")] {
                let response = serve(root.path().into(), path, accept).await.unwrap();
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                assert_eq!(
                    response.headers()[header::CONTENT_TYPE],
                    "text/html; charset=utf-8"
                );
                assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let expected = if accept.is_some() {
                    gzip("首页".as_bytes())
                } else {
                    "首页".as_bytes().to_vec()
                };
                assert_eq!(bytes.as_ref(), expected);
            }
        }
        let response = serve(root.path().into(), "/style.css", Some("gzip"))
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        assert!(!response.headers().contains_key(header::VARY));
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "body{}"
        );

        let response = serve(root.path().into(), "/index.html.gz", Some("gzip"))
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        assert!(!response.headers().contains_key(header::VARY));
        assert_eq!(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .as_ref(),
            gzip("首页".as_bytes())
        );
    }

    #[tokio::test]
    async fn 只有压缩文件而原文件与首页都不存在时仍返回缺失() {
        let root = crate::test_support::tempdir();
        std::fs::write(root.path().join("missing.js.gz"), gzip(b"test")).unwrap();
        assert_eq!(
            serve(root.path().into(), "/missing.js", Some("gzip"))
                .await
                .err()
                .unwrap()
                .code,
            "mobile.not_found"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn 压缩文件符号链接越界时两种编码请求均被拒绝() {
        let root = crate::test_support::tempdir();
        let assets = root.path().join("assets");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("app.js"), "原文件").unwrap();
        let outside = root.path().join("outside.gz");
        std::fs::write(&outside, "目录外的内容").unwrap();
        std::os::unix::fs::symlink(&outside, assets.join("app.js.gz")).unwrap();
        for accept in [None, Some("gzip"), Some("gzip;q=0")] {
            for path in ["/app.js", "/app.js.gz"] {
                assert_eq!(
                    serve(assets.clone(), path, accept)
                        .await
                        .err()
                        .unwrap()
                        .code,
                    "mobile.invalid_path"
                );
            }
        }
    }
}
