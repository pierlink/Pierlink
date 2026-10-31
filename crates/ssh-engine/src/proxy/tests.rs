use super::*;
use crate::test_support::proxy::{Reply, TestProxy};

fn server(kind: ProxyKind) -> ProxyServer {
    ProxyServer {
        kind,
        host: "proxy.example".into(),
        port: 8080,
    }
}

#[test]
fn 拒绝原因提供完整翻译键并保持中文显示() {
    use RefuseReason::*;
    for (reason, key, text) in [
        (
            ConnectionUnavailable,
            "proxy.connection_unavailable",
            "无法取得代理连接",
        ),
        (
            ConnectionSetupFailed,
            "proxy.connection_setup_failed",
            "无法设置代理连接",
        ),
        (
            RequestWriteFailed,
            "proxy.request_write_failed",
            "写入代理请求失败",
        ),
        (
            ResponseIncomplete,
            "proxy.response_incomplete",
            "代理响应不完整",
        ),
        (
            ResponseHeaderTooLarge,
            "proxy.response_header_too_large",
            "代理响应头过大",
        ),
        (
            InvalidHttpStatusLine,
            "proxy.invalid_http_status_line",
            "代理 HTTP 状态行无效",
        ),
        (
            InvalidHttpStatus,
            "proxy.invalid_http_status",
            "代理 HTTP 状态码无效",
        ),
        (HttpStatus(403), "proxy.http_status", "HTTP 状态码 403"),
        (
            SocksHandshakeInterrupted,
            "proxy.socks_handshake_interrupted",
            "SOCKS5 握手中断",
        ),
        (
            InvalidSocksVersion,
            "proxy.invalid_socks_version",
            "SOCKS5 协议版本无效",
        ),
        (
            InvalidSocksAuthMethod,
            "proxy.invalid_socks_auth_method",
            "SOCKS5 认证方式无效",
        ),
        (
            InvalidSocksResponse,
            "proxy.invalid_socks_response",
            "SOCKS5 响应格式无效",
        ),
        (SocksReply(2), "proxy.socks_reply", "SOCKS5 回复码 2"),
        (
            InvalidSocksAddressType,
            "proxy.invalid_socks_address_type",
            "SOCKS5 地址类型无效",
        ),
    ] {
        assert_eq!(reason.key(), key);
        assert_eq!(reason.to_string(), text);
        assert_eq!(
            refused("2001:db8::1", 22, reason).to_string(),
            format!("代理拒绝连接目标 [2001:db8::1]:22：{text}")
        );
    }
}

#[test]
fn 地址校验拒绝凭据及非法输入() {
    for kind in [ProxyKind::Http, ProxyKind::Socks5] {
        let mut proxy = server(kind);
        assert!(validate(&proxy, "unresolved.invalid", 22).is_ok());
        assert!(validate(&proxy, "::1", 22).is_ok());
        for host in ["", "bad\r\nHost:x", "user:secret@proxy", "bad host", "a/b"] {
            assert_eq!(validate(&proxy, host, 22), Err(ProxyError::InvalidInput));
        }
        assert_eq!(validate(&proxy, "host", 0), Err(ProxyError::InvalidInput));
        proxy.port = 0;
        assert_eq!(validate(&proxy, "host", 22), Err(ProxyError::InvalidInput));
        proxy.port = 8080;
        proxy.host = "user:secret@proxy".into();
        assert_eq!(validate(&proxy, "host", 22), Err(ProxyError::InvalidInput));
    }
    assert_eq!(authority("::1", 22), "[::1]:22");
    assert_eq!(authority("host", 22), "host:22");
}

#[tokio::test]
async fn 内存http无认证请求目标格式有界头部和状态码() {
    for host in ["unresolved.invalid", "192.0.2.1", "2001:db8::1"] {
        for (response, code) in [
            (
                b"HTTP/1.1 204 OK\r\nHeader: value\r\n\r\nSSH-banner".as_slice(),
                None,
            ),
            (b"HTTP/1.1 407 secret-reason\r\n\r\nsecret-body", Some(407)),
            (b"HTTP/1.1 403 secret-reason\r\n\r\nsecret-body", Some(403)),
            (b"HTTP/1.1 bad secret-reason\r\n\r\n", Some(0)),
            (b"invalid secret-reason\r\n\r\n", Some(1)),
        ] {
            let target = if host.contains(':') {
                format!("[{host}]:22")
            } else {
                format!("{host}:22")
            };
            let mut expected = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
            expected.push_str("\r\n");
            let (mut client, mut peer) = tokio::io::duplex(32768);
            let remote = async {
                let mut received = vec![0; expected.len()];
                peer.read_exact(&mut received).await.unwrap();
                assert_eq!(received, expected.as_bytes());
                peer.write_all(response).await.unwrap();
            };
            let (result, ()) = tokio::join!(http(&mut client, host, 22), remote);
            match code {
                None => {
                    result.unwrap();
                    let mut banner = [0; 10];
                    client.read_exact(&mut banner).await.unwrap();
                    assert_eq!(&banner, b"SSH-banner");
                }
                Some(407) => assert_eq!(result, Err(ProxyError::AuthRequired)),
                Some(code) => {
                    let error = result.unwrap_err();
                    let reason = match code {
                        0 => RefuseReason::InvalidHttpStatus,
                        1 => RefuseReason::InvalidHttpStatusLine,
                        status => RefuseReason::HttpStatus(status),
                    };
                    assert_eq!(error, refused(host, 22, reason));
                    assert!(!format!("{error:?}").contains("secret"));
                    assert!(!error.to_string().contains("secret"));
                }
            }
        }
    }

    for response in [vec![b'x'; 17000], b"HTTP/1.1 200 truncated".to_vec()] {
        let (mut client, mut peer) = tokio::io::duplex(32768);
        let remote = async {
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(peer.read_u8().await.unwrap());
            }
            peer.write_all(&response).await.unwrap();
            peer.shutdown().await.unwrap();
        };
        let (result, ()) = tokio::join!(http(&mut client, "host", 22), remote);
        let expected = if response.len() > 16384 {
            RefuseReason::ResponseHeaderTooLarge
        } else {
            RefuseReason::ResponseIncomplete
        };
        assert_eq!(result, Err(refused("host", 22, expected)));
    }
}

#[tokio::test]
async fn 内存代理流中断及畸形socks响应返回固定原因() {
    let (mut client, peer) = tokio::io::duplex(1024);
    drop(peer);
    assert_eq!(
        http(&mut client, "host", 22).await,
        Err(refused("host", 22, RefuseReason::RequestWriteFailed))
    );
    assert_eq!(
        socks(&mut client, "host", 22).await,
        Err(refused("host", 22, RefuseReason::SocksHandshakeInterrupted))
    );
    for (response, reason) in [
        (vec![4, 0, 0, 1], RefuseReason::InvalidSocksResponse),
        (vec![5, 0, 1, 1], RefuseReason::InvalidSocksResponse),
        (vec![5, 0, 0, 9], RefuseReason::InvalidSocksAddressType),
        (vec![5, 0], RefuseReason::SocksHandshakeInterrupted),
    ] {
        let (mut client, mut peer) = tokio::io::duplex(1024);
        peer.write_all(&[5, 0]).await.unwrap();
        peer.write_all(&response).await.unwrap();
        peer.shutdown().await.unwrap();
        assert_eq!(
            socks(&mut client, "host", 22).await,
            Err(refused("host", 22, reason))
        );
    }
}

#[tokio::test]
async fn 内存socks无认证地址类型错误回复及残留banner() {
    for (host, address) in [
        (
            "unresolved.invalid",
            [vec![3, 18], b"unresolved.invalid".to_vec()].concat(),
        ),
        ("192.0.2.1", vec![1, 192, 0, 2, 1]),
        (
            "2001:db8::1",
            [
                vec![4],
                "2001:db8::1"
                    .parse::<std::net::Ipv6Addr>()
                    .unwrap()
                    .octets()
                    .to_vec(),
            ]
            .concat(),
        ),
    ] {
        for reply_code in [0, 1, 2, 3, 4, 5, 6, 7, 8] {
            let (mut client, mut peer) = tokio::io::duplex(1024);
            let remote = async {
                let mut greeting = [0; 3];
                peer.read_exact(&mut greeting).await.unwrap();
                assert_eq!(greeting, [5, 1, 0]);
                peer.write_all(&[5, 0]).await.unwrap();
                let expected = [vec![5, 1, 0], address.clone(), vec![0, 22]].concat();
                let mut received = vec![0; expected.len()];
                peer.read_exact(&mut received).await.unwrap();
                assert_eq!(received, expected);
                peer.write_all(&[5, reply_code, 0, 3, 1, b'a', 0, 0])
                    .await
                    .unwrap();
                peer.write_all(b"SSH-banner").await.unwrap();
            };
            let (result, ()) = tokio::join!(socks(&mut client, host, 22), remote);
            if reply_code == 0 {
                result.unwrap();
                let mut banner = [0; 10];
                client.read_exact(&mut banner).await.unwrap();
                assert_eq!(&banner, b"SSH-banner");
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    refused(host, 22, RefuseReason::SocksReply(reply_code))
                );
            }
        }
    }

    for (response, expected) in [
        (vec![5, 255], ProxyError::AuthRequired),
        (vec![5, 2], ProxyError::AuthRequired),
        (
            vec![5, 1],
            refused("host", 22, RefuseReason::InvalidSocksAuthMethod),
        ),
        (
            vec![4, 0],
            refused("host", 22, RefuseReason::InvalidSocksVersion),
        ),
    ] {
        let (mut client, mut peer) = tokio::io::duplex(1024);
        peer.write_all(&response).await.unwrap();
        assert_eq!(socks(&mut client, "host", 22).await, Err(expected));
    }
    let proxy = server(ProxyKind::Socks5);
    assert_eq!(
        connect_async(&proxy, "host", 22, Duration::ZERO)
            .await
            .unwrap_err(),
        ProxyError::Timeout
    );
}

#[test]
#[ignore = "需要本机 TCP 端口"]
fn 同步异步无认证握手及域名和ip地址不丢banner() {
    use std::io::Read;
    for kind in [ProxyKind::Http, ProxyKind::Socks5] {
        for host in ["unresolved.invalid", "192.0.2.1", "2001:db8::1"] {
            let proxy = TestProxy::start(kind, Reply::Banner).unwrap();
            let mut stream = connect(&proxy.server, host, 22, Duration::from_secs(2)).unwrap();
            let mut banner = String::new();
            stream.read_to_string(&mut banner).unwrap();
            assert_eq!(banner, "SSH-2.0-test\r\n");
            assert_eq!(*proxy.targets.lock().unwrap(), [(host.into(), 22)]);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                // 同步接口也能在 Tauri 的 tokio 上下文中调用。
                let mut blocking =
                    connect(&proxy.server, host, 24, Duration::from_secs(2)).unwrap();
                let mut banner = String::new();
                std::io::Read::read_to_string(&mut blocking, &mut banner).unwrap();
                assert_eq!(banner, "SSH-2.0-test\r\n");
                let mut stream = connect_async(&proxy.server, host, 23, Duration::from_secs(2))
                    .await
                    .unwrap();
                let mut banner = String::new();
                stream.read_to_string(&mut banner).await.unwrap();
                assert_eq!(banner, "SSH-2.0-test\r\n");
            });
        }
    }
}

#[test]
#[ignore = "需要本机 TCP 端口"]
fn 代理要求认证拒绝超时及恶意响应均受控() {
    for kind in [ProxyKind::Http, ProxyKind::Socks5] {
        let proxy = TestProxy::start(kind, Reply::AuthRequired).unwrap();
        assert_eq!(
            connect(&proxy.server, "host", 22, Duration::from_secs(1)).unwrap_err(),
            ProxyError::AuthRequired
        );
        for reply in [
            Reply::Refuse,
            Reply::Malformed,
            Reply::Oversized,
            Reply::Stall,
        ] {
            let proxy = TestProxy::start(kind, reply).unwrap();
            let error = connect(&proxy.server, "host", 22, Duration::from_millis(100)).unwrap_err();
            if matches!(reply, Reply::Stall) {
                assert_eq!(error, ProxyError::Timeout);
            } else {
                assert!(matches!(error, ProxyError::Refused { .. }));
            }
            assert!(!format!("{error:?}").contains("secret"));
        }
        let proxy = TestProxy::start(kind, Reply::Banner).unwrap();
        let address = proxy.server.clone();
        drop(proxy);
        assert!(matches!(
            connect(&address, "host", 22, Duration::from_secs(1)),
            Err(ProxyError::Unreachable { .. })
        ));
    }
}

#[cfg(unix)]
#[test]
#[ignore = "需要本机 sshd 和 TCP 端口"]
fn 内置ssh经两种代理及跳板连接并保持进度顺序() {
    use crate::{ConnectStage, Engine};
    use std::sync::{Arc, Mutex};
    let sshd = crate::test_files::Server::new("");
    for kind in [ProxyKind::Http, ProxyKind::Socks5] {
        let proxy =
            TestProxy::start(kind, Reply::Forward(([127, 0, 0, 1], sshd.port).into())).unwrap();
        let mut options = sshd.options();
        options.host = "unresolved.invalid".into();
        options.proxy = Some(proxy.server.clone());
        let stages = Arc::new(Mutex::new(Vec::new()));
        let observed = stages.clone();
        options.progress = Some(Arc::new(move |stage| observed.lock().unwrap().push(stage)));
        let engine = Engine::new().unwrap();
        let connection = engine.connect(options, sshd.auth()).unwrap();
        assert_eq!(
            crate::test_files::command(&connection, "printf proxy-ok"),
            b"proxy-ok"
        );
        assert_eq!(
            *proxy.targets.lock().unwrap(),
            [("unresolved.invalid".into(), sshd.port)]
        );
        assert_eq!(
            *stages.lock().unwrap(),
            [
                ConnectStage::Resolve,
                ConnectStage::Connect,
                ConnectStage::Auth
            ]
        );
        let mut next = sshd.options();
        next.proxy = Some(server(ProxyKind::Http));
        let via = engine.connect_via(&connection, next, sshd.auth()).unwrap();
        assert_eq!(crate::test_files::command(&via, "printf via-ok"), b"via-ok");
    }
}
