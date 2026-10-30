//! 管理页会话只发往本机回环，不使用代理、不跟随重定向，不保留响应正文。
use super::client::loopback_port;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 交给隐藏管理窗口设置的会话 cookie；仅保存在内存中，关闭窗口后注销并删除。
#[derive(Clone, Serialize, Deserialize, specta::Type)]
pub struct GuiCookie {
    pub name: String,
    pub value: String,
    pub path: String,
    /// 不设置 Domain，使用此 URL 创建仅限该主机的 cookie。
    pub url: String,
    pub http_only: bool,
    pub secure: bool,
    pub same_site: Option<String>,
}
impl std::fmt::Debug for GuiCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuiCookie")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn request(
    base: &str,
    endpoint: &str,
    body: Vec<u8>,
    cookie: Option<&GuiCookie>,
) -> Result<ureq::http::Response<ureq::Body>, AppError> {
    loopback_port(base, "http://127.0.0.1:")?;
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build(),
    );
    let mut builder = ureq::http::Request::builder()
        .method("POST")
        .uri(format!("{base}/rest/noauth/auth/{endpoint}"))
        .header("Content-Type", "application/json");
    if let Some(cookie) = cookie {
        validate_cookie(cookie, base)?;
        builder = builder.header("Cookie", format!("{}={}", cookie.name, cookie.value));
    }
    let request = builder
        .body(body)
        .map_err(|_| AppError::new("syncthing.invalid_cookie"))?;
    agent
        .run(request)
        .map_err(|_| AppError::new("syncthing.unreachable"))
}
fn status(code: u16) -> Result<(), AppError> {
    match code {
        200..=299 => Ok(()),
        401 | 403 => Err(AppError::new("syncthing.unauthorized")),
        404 | 405 => Err(AppError::new("syncthing.gui_auth_unsupported").with_reason(
            "syncthing.this_version_does_not_support_management_page_session_login",
            "此版本不支持管理页会话登录接口",
        )),
        _ => Err(AppError::new("syncthing.http_error").with("status", code)),
    }
}
fn validate_cookie(cookie: &GuiCookie, base: &str) -> Result<(), AppError> {
    let suffix = cookie.name.strip_prefix("sessionid-").unwrap_or("");
    if cookie.url != base
        || suffix.is_empty()
        || suffix.len() > 64
        || !suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        || cookie.value.is_empty()
        || cookie.value.len() > 4096
        || !cookie
            .value
            .bytes()
            .all(|b| b.is_ascii_graphic() && !b"\";,\\".contains(&b))
        || cookie.path != "/"
        || cookie.secure
    {
        return Err(AppError::new("syncthing.invalid_cookie"));
    }
    Ok(())
}
fn parse_cookie(base: &str, headers: &ureq::http::HeaderMap) -> Result<GuiCookie, AppError> {
    for value in headers.get_all("set-cookie") {
        let value = value
            .to_str()
            .map_err(|_| AppError::new("syncthing.invalid_cookie"))?;
        let mut parts = value.split(';').map(str::trim);
        let Some((name, value)) = parts.next().and_then(|v| v.split_once('=')) else {
            continue;
        };
        if !name.starts_with("sessionid-") {
            continue;
        }
        let mut cookie = GuiCookie {
            name: name.into(),
            value: value.into(),
            path: "/".into(),
            url: base.into(),
            http_only: false,
            secure: false,
            same_site: None,
        };
        for part in parts {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            match key.to_ascii_lowercase().as_str() {
                "path" => cookie.path = value.into(),
                "httponly" => cookie.http_only = true,
                "secure" => cookie.secure = true,
                "samesite"
                    if ["strict", "lax", "none"].contains(&value.to_ascii_lowercase().as_str()) =>
                {
                    cookie.same_site = Some(value.to_ascii_lowercase())
                }
                "domain" => return Err(AppError::new("syncthing.invalid_cookie")),
                _ => {}
            }
        }
        validate_cookie(&cookie, base)?;
        return Ok(cookie);
    }
    Err(AppError::new("syncthing.gui_auth_unsupported").with_reason(
        "syncthing.login_response_has_no_session_cookie",
        "登录响应没有会话 cookie",
    ))
}

/// 使用 base（http://127.0.0.1:端口）、username、password 登录；返回短期会话 cookie。
/// 旧版缺少会话接口时返回 gui_auth_unsupported，禁止退回 URL 内嵌凭据或持久登录。
pub fn login(base: &str, username: &str, password: &str) -> Result<GuiCookie, AppError> {
    let body = serde_json::to_vec(
        &serde_json::json!({"username":username,"password":password,"stayLoggedIn":false}),
    )
    .map_err(|_| AppError::new("syncthing.invalid_config"))?;
    let response = request(base, "password", body, None)?;
    status(response.status().as_u16())?;
    parse_cookie(base, response.headers())
}

/// 用 login 返回的 cookie 注销 base 上的会话；返回结果后调用方仍应删除窗口 cookie。
pub fn logout(base: &str, cookie: &GuiCookie) -> Result<(), AppError> {
    let response = request(base, "logout", Vec::new(), Some(cookie))?;
    status(response.status().as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 会话解析兼容属性并隐藏秘密() {
        let mut headers = ureq::http::HeaderMap::new();
        headers.append("set-cookie", "CSRF-ABCD=ignored; Path=/".parse().unwrap());
        headers.append(
            "set-cookie",
            "sessionid-ABCDEFG=secret; Path=/; HttpOnly; SameSite=Lax"
                .parse()
                .unwrap(),
        );
        let cookie = parse_cookie("http://127.0.0.1:1234", &headers).unwrap();
        assert!(cookie.http_only);
        assert_eq!(cookie.same_site.as_deref(), Some("lax"));
        assert!(!format!("{cookie:?}").contains("secret"));
        assert_eq!(
            validate_cookie(&cookie, "http://127.0.0.1:5678")
                .unwrap_err()
                .code,
            "syncthing.invalid_cookie"
        );
        for code in [302, 401, 403, 404, 405, 500] {
            assert!(status(code).is_err());
        }
        assert!(status(204).is_ok());
        assert!(parse_cookie("http://127.0.0.1:1", &Default::default()).is_err());
    }
    #[test]
    fn 登录注销拒绝外部地址和注入() {
        for base in [
            "http://evil:1",
            "http://127.0.0.1:1@evil",
            "http://127.0.0.1:1/path",
        ] {
            assert!(login(base, "user", "secret").is_err());
        }
        let base = "http://127.0.0.1:1";
        let mut headers = ureq::http::HeaderMap::new();
        headers.insert(
            "set-cookie",
            "sessionid-ABCDEFG=secret; Path=/".parse().unwrap(),
        );
        let mut cookie = parse_cookie(base, &headers).unwrap();
        cookie.value.push_str("\r\nHost: evil");
        assert_eq!(
            logout(base, &cookie).unwrap_err().code,
            "syncthing.invalid_cookie"
        );
        assert!(!format!("{:?}", logout(base, &cookie)).contains("secret"));
    }

    #[test]
    #[ignore = "需要允许本机随机监听端口"]
    fn 本机服务验证登录正文注销及重定向不泄露凭据() {
        use std::{
            io::{BufRead, BufReader, Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let peer = std::thread::spawn(move || {
            for index in 0..3 {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut input = BufReader::new(stream);
                let mut request = String::new();
                input.read_line(&mut request).unwrap();
                let mut size = 0;
                let mut sent_cookie = false;
                loop {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        size = value.trim().parse::<usize>().unwrap();
                    }
                    if lower.starts_with("cookie:") {
                        sent_cookie = line.contains("sessionid-ABCDEFG=session-secret");
                    }
                    assert!(!lower.starts_with("authorization:"));
                }
                let mut body = vec![0; size];
                input.read_exact(&mut body).unwrap();
                if index == 1 {
                    assert!(request.starts_with("POST /rest/noauth/auth/logout "));
                    assert!(sent_cookie);
                } else {
                    assert!(request.starts_with("POST /rest/noauth/auth/password "));
                    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(value["username"], "user");
                    assert_eq!(value["password"], "password-secret");
                    assert_eq!(value["stayLoggedIn"], false);
                }
                let response = if index == 0 {
                    "HTTP/1.1 200 OK\r\nSet-Cookie: sessionid-ABCDEFG=session-secret; Path=/; HttpOnly\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else if index == 1 {
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 302 Found\r\nLocation: http://example.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                input.get_mut().write_all(response.as_bytes()).unwrap();
            }
        });
        let cookie = login(&base, "user", "password-secret").unwrap();
        logout(&base, &cookie).unwrap();
        assert_eq!(
            login(&base, "user", "password-secret").unwrap_err().params["status"],
            "302"
        );
        peer.join().unwrap();
    }
}
