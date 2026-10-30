//! 设备地址只使用固定回环端口；刷新与重拨共用锁，避免覆盖彼此的协议方案。
use super::invalid_response;
use crate::{
    error::AppError,
    syncthing::client::{Client, loopback_port},
};
use serde_json::json;
use std::{sync::Mutex, time::Duration};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

/// 根据当前回环地址和目标端口生成地址；redial 为真时切换方案，返回地址或格式错误。
pub(super) fn device_address(current: &str, port: u16, redial: bool) -> Result<String, AppError> {
    let scheme = if current.starts_with("tcp4://") {
        "tcp4"
    } else {
        "tcp"
    };
    loopback_port(current, &format!("{scheme}://127.0.0.1:"))?;
    if port == 0 {
        return Err(invalid_response());
    }
    let scheme = match (scheme, redial) {
        ("tcp", true) | ("tcp4", false) => "tcp4",
        _ => "tcp",
    };
    Ok(format!("{scheme}://127.0.0.1:{port}"))
}

/// 在 updates 锁内读取 client 中 id 的当前地址并更新端口；redial 指定是否切换方案，返回更新结果。
/// 地址配置不是单个有效回环地址时，修正为单个 tcp 回环地址并记录日志。
pub(super) fn update_device_address(
    client: &Client,
    updates: &Mutex<()>,
    id: &str,
    port: u16,
    redial: bool,
) -> Result<(), AppError> {
    if port == 0 {
        return Err(invalid_response());
    }
    let _guard = updates.lock().map_err(|_| invalid_response())?;
    let path = format!("/rest/config/devices/{id}");
    let device = client.request("GET", &path, &[], None, REQUEST_TIMEOUT)?;
    let addresses = device
        .as_object()
        .ok_or_else(invalid_response)?
        .get("addresses");
    let current = addresses
        .and_then(|value| value.as_array())
        .filter(|values| values.len() == 1)
        .and_then(|values| values[0].as_str());
    let expected = current.and_then(|value| device_address(value, port, redial).ok());
    let repaired = expected.is_none();
    let address = expected.unwrap_or_else(|| format!("tcp://127.0.0.1:{port}"));
    if current != Some(address.as_str()) {
        // Syncthing 2.1.7 的 tcp/tcp4 共用 TCP 拨号器和优先级；地址变更只唤醒拨号循环，
        // 不使用暂停恢复触发的设备强制重拨额度，也不改文件夹或索引。
        client.request(
            "PATCH",
            &path,
            &[],
            Some(json!({"addresses": [address]})),
            REQUEST_TIMEOUT,
        )?;
        if repaired {
            // 不记录外部地址内容，避免把配置中夹带的凭据写入日志。
            eprintln!("设备地址配置异常，已修正为单个 IPv4 回环地址");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 同端口切换方案_换端口刷新保留方案() {
        for (current, refreshed, redialed) in [
            (
                "tcp://127.0.0.1:12345",
                "tcp://127.0.0.1:23456",
                "tcp4://127.0.0.1:23456",
            ),
            (
                "tcp4://127.0.0.1:12345",
                "tcp4://127.0.0.1:23456",
                "tcp://127.0.0.1:23456",
            ),
        ] {
            assert_eq!(device_address(current, 23456, false).unwrap(), refreshed);
            assert_eq!(device_address(current, 23456, true).unwrap(), redialed);
            let next = device_address(current, 12345, true).unwrap();
            assert_eq!(device_address(&next, 12345, true).unwrap(), current);
        }
    }

    #[test]
    fn 非法或非回环地址不生成拨号地址() {
        for current in [
            "dynamic",
            "tcp://example.com:12345",
            "tcp://127.0.0.1:0",
            "tcp4://127.0.0.1:70000",
            "tcp6://127.0.0.1:12345",
        ] {
            assert!(device_address(current, 23456, true).is_err());
        }
        assert!(device_address("tcp://127.0.0.1:12345", 0, true).is_err());
    }

    #[test]
    fn 并发刷新与重拨在锁内读写_不覆盖当前方案() {
        use std::sync::{Arc, Barrier};
        let updates = Arc::new(Mutex::new(()));
        let lock = updates.clone();
        let address = Arc::new(Mutex::new("tcp://127.0.0.1:12345".to_owned()));
        let current = address.clone();
        let mut client = Client::new("http://127.0.0.1:1", "test-key").unwrap();
        client.test_handler = Some(Arc::new(move |method, path, _, body| {
            assert_eq!(path, "/rest/config/devices/服务器");
            assert!(lock.try_lock().is_err(), "读取和写入必须共用地址更新锁");
            match method {
                "GET" => Ok(json!({"addresses": [*current.lock().unwrap()]})),
                "PATCH" => {
                    *current.lock().unwrap() =
                        body.unwrap()["addresses"][0].as_str().unwrap().into();
                    Ok(serde_json::Value::Null)
                }
                _ => panic!("只允许读取和修改目标设备地址"),
            }
        }));
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            for redial in [false, true] {
                let (client, updates, barrier) = (&client, &updates, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    update_device_address(client, updates, "服务器", 23456, redial).unwrap();
                });
            }
        });
        assert_eq!(*address.lock().unwrap(), "tcp4://127.0.0.1:23456");
    }

    #[test]
    fn 地址配置异常时修正为单个回环地址() {
        use std::sync::Arc;
        for value in [
            json!({}),
            json!({"addresses": []}),
            json!({"addresses": [12]}),
            json!({"addresses": ["dynamic", "tcp://127.0.0.1:12345"]}),
            json!({"addresses": ["tcp://127.0.0.1:12345", "tcp4://127.0.0.1:23456"]}),
            json!({"addresses": ["dynamic"]}),
            json!({"addresses": ["tcp://example.com:12345"]}),
            json!({"addresses": ["tcp6://[::1]:12345"]}),
            json!({"addresses": ["tcp4://127.0.0.1:0"]}),
            json!({"addresses": ["tcp://127.0.0.1:70000"]}),
            json!({"addresses": null}),
            json!({"addresses": "dynamic"}),
        ] {
            let patches = Arc::new(Mutex::new(vec![]));
            let recorded = patches.clone();
            let mut client = Client::new("http://127.0.0.1:1", "test-key").unwrap();
            client.test_handler = Some(Arc::new(move |method, path, _, body| {
                assert_eq!(path, "/rest/config/devices/服务器");
                match method {
                    "GET" => Ok(value.clone()),
                    "PATCH" => {
                        recorded.lock().unwrap().push(body.unwrap());
                        Ok(serde_json::Value::Null)
                    }
                    _ => panic!("只允许读取和修正目标设备地址"),
                }
            }));
            for redial in [false, true] {
                update_device_address(&client, &Mutex::new(()), "服务器", 23456, redial).unwrap();
            }
            assert_eq!(
                *patches.lock().unwrap(),
                vec![json!({"addresses": ["tcp://127.0.0.1:23456"]}); 2]
            );
        }
    }

    #[test]
    fn 地址已匹配无需写入_配置读取或写入失败仍返回错误() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for case in ["已匹配", "读取失败", "配置响应异常", "写入失败", "零端口"]
        {
            let writes = Arc::new(AtomicUsize::new(0));
            let count = writes.clone();
            let mut client = Client::new("http://127.0.0.1:1", "test-key").unwrap();
            client.test_handler = Some(Arc::new(move |method, _, _, _| {
                if case == "读取失败" || method == "PATCH" {
                    if method == "PATCH" {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                    return Err(AppError::new("syncthing.unreachable"));
                }
                Ok(match case {
                    "配置响应异常" => json!(null),
                    "已匹配" => json!({"addresses": ["tcp4://127.0.0.1:23456"]}),
                    _ => json!({"addresses": []}),
                })
            }));
            let result = update_device_address(
                &client,
                &Mutex::new(()),
                "服务器",
                if case == "零端口" { 0 } else { 23456 },
                false,
            );
            match case {
                "已匹配" => result.unwrap(),
                "读取失败" | "写入失败" => {
                    assert_eq!(result.unwrap_err().code, "syncthing.unreachable")
                }
                _ => assert_eq!(result.unwrap_err().code, "sync.invalid_response"),
            }
            assert_eq!(
                writes.load(Ordering::Relaxed),
                usize::from(case == "写入失败")
            );
        }
    }
}
