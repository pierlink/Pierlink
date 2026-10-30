//! 仅向回环接口发送认证请求；禁用代理和重定向，响应有大小与时间上限。
use super::{
    error,
    folder::{FolderSpec, validate_device_id, validate_ignores, validate_task_folder_id},
};
use crate::{error::AppError, security::redact};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, time::Duration};

const MAX_BODY: usize = 16 * 1024 * 1024;
#[cfg(test)]
type TestHandler = std::sync::Arc<
    dyn Fn(&str, &str, &[(&str, &str)], Option<Value>) -> Result<Value, AppError> + Send + Sync,
>;
#[derive(Clone)]
pub struct Client {
    #[cfg(test)]
    pub(crate) test_handler: Option<TestHandler>,
    base: String,
    api_key: String,
    #[cfg(unix)]
    socket: Option<std::path::PathBuf>,
}
#[derive(Debug, Deserialize)]
pub struct Event {
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub time: String,
    pub data: Value,
}
#[derive(Debug)]
pub struct EventBatch {
    pub events: Vec<Event>,
    pub last_id: u64,
}
fn invalid() -> AppError {
    AppError::new("syncthing.invalid_response")
}
pub(crate) fn loopback_port(value: &str, prefix: &str) -> Result<u16, AppError> {
    let port = value.strip_prefix(prefix).ok_or_else(|| {
        error(
            "syncthing.only_local_ipv4_loopback_addresses_are_allowed",
            "只允许本机 IPv4 回环地址",
        )
    })?;
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(error("syncthing.invalid_port", "端口无效"));
    }
    port.parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| error("syncthing.invalid_port", "端口无效"))
}
fn encode(value: &str) -> String {
    let mut result = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            result.push(b as char);
        } else {
            result.push_str(&format!("%{b:02X}"));
        }
    }
    result
}
impl Client {
    pub(crate) fn redact_text(&self, text: &str) -> String {
        redact(text, &[&self.api_key])
    }
    /// 用 base（仅 http://127.0.0.1:端口）和 api_key 创建客户端；非法地址或认证头返回错误。
    pub fn new(base: &str, api_key: &str) -> Result<Self, AppError> {
        loopback_port(base, "http://127.0.0.1:")?;
        if api_key.bytes().any(|b| b < 32 || b == 127) {
            return Err(error(
                "syncthing.api_key_contains_invalid_control_characters",
                "API 密钥含非法控制字符",
            ));
        }
        Ok(Self {
            #[cfg(test)]
            test_handler: None,
            base: base.into(),
            api_key: api_key.into(),
            #[cfg(unix)]
            socket: None,
        })
    }
    #[cfg(unix)]
    pub(crate) fn unix(path: &std::path::Path, api_key: &str) -> Result<Self, AppError> {
        let mut client = Self::new("http://127.0.0.1:1", api_key)?;
        client.socket = Some(path.into());
        Ok(client)
    }
    pub(crate) fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, AppError> {
        #[cfg(test)]
        if let Some(handler) = &self.test_handler {
            return handler(method, path, query, body);
        }
        let query = query
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!(
            "{}{path}{}{}",
            self.base,
            if query.is_empty() { "" } else { "?" },
            query
        );
        let config = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(5).min(timeout)))
            .timeout_global(Some(timeout))
            .build();
        #[cfg(unix)]
        let agent = if let Some(path) = &self.socket {
            ureq::Agent::with_parts(
                config,
                super::unix::Connector(path.clone()),
                ureq::unversioned::resolver::DefaultResolver::default(),
            )
        } else {
            ureq::Agent::new_with_config(config)
        };
        #[cfg(not(unix))]
        let agent = ureq::Agent::new_with_config(config);
        let bytes = body
            .map(|v| serde_json::to_vec(&v))
            .transpose()
            .map_err(|_| error("syncthing.couldn_t_encode_the_request", "无法编码请求"))?
            .unwrap_or_default();
        let req = ureq::http::Request::builder()
            .method(method)
            .uri(url)
            .header("X-API-Key", &self.api_key)
            .header("Content-Type", "application/json")
            .body(bytes)
            .map_err(|_| error("syncthing.couldn_t_build_the_request", "无法构造请求"))?;
        let mut response = agent
            .run(req)
            .map_err(|_| AppError::new("syncthing.unreachable"))?;
        let status = response.status().as_u16();
        if status == 403 {
            return Err(AppError::new("syncthing.unauthorized"));
        }
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take((MAX_BODY + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| AppError::new("syncthing.unreachable"))?;
        decode(status, &bytes, &self.api_key)
    }
    pub(crate) fn call(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<Value, AppError> {
        self.request(method, path, query, body, Duration::from_secs(30))
    }
    pub(crate) fn ping_with_timeout(&self, timeout: Duration) -> Result<(), AppError> {
        let value = self.request("GET", "/rest/system/ping", &[], None, timeout)?;
        if value["ping"] != "pong" {
            return Err(invalid());
        }
        Ok(())
    }
    pub(crate) fn listeners_ready(&self, timeout: Duration) -> Result<bool, AppError> {
        let status = self.request("GET", "/rest/system/status", &[], None, timeout)?;
        self.validate_listeners(&status)
    }
    fn validate_listeners(&self, status: &Value) -> Result<bool, AppError> {
        let Some(listeners) = status["connectionServiceStatus"].as_object() else {
            return Ok(false);
        };
        if listeners.is_empty() {
            return Ok(false);
        }
        if listeners
            .keys()
            .any(|key| !key.starts_with("tcp://127.0.0.1:"))
        {
            return Err(AppError::new("syncthing.listen_address_mismatch"));
        }
        for listener in listeners.values() {
            if let Some(reason) = listener["error"].as_str().filter(|v| !v.is_empty()) {
                let code = if super::port_in_use(reason) {
                    "syncthing.listen_port_in_use"
                } else {
                    "syncthing.start_failed"
                };
                return Err(AppError::new(code).with(
                    "reason",
                    redact(reason, &[&self.api_key])
                        .chars()
                        .take(512)
                        .collect::<String>(),
                ));
            }
            if !listener["lanAddresses"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// 无参数；确认接口返回 pong，失败返回协议或连接错误。
    pub fn ping(&self) -> Result<(), AppError> {
        self.ping_with_timeout(Duration::from_secs(30))
    }
    /// 无参数；返回版本字符串。
    pub fn version(&self) -> Result<String, AppError> {
        self.call("GET", "/rest/system/version", &[], None)?["version"]
            .as_str()
            .map(String::from)
            .ok_or_else(invalid)
    }
    /// 无参数；返回当前设备 ID 并校验格式。
    pub fn my_device_id(&self) -> Result<String, AppError> {
        let v = self.call("GET", "/rest/system/status", &[], None)?;
        let id = v["myID"].as_str().ok_or_else(invalid)?;
        validate_device_id(id).map_err(|_| invalid())?;
        Ok(id.into())
    }
    /// 按设备 id、名称 name 和回环地址或 dynamic 的 addresses 添加或更新设备；关闭自动接受与介绍人。
    pub fn add_device(&self, id: &str, name: &str, addresses: &[String]) -> Result<(), AppError> {
        validate_device_id(id)?;
        if addresses.is_empty() {
            return Err(error(
                "syncthing.device_must_have_a_loopback_address",
                "设备必须有回环地址",
            ));
        }
        for addr in addresses {
            if addr != "dynamic" {
                loopback_port(addr, "tcp://127.0.0.1:")?;
            }
        }
        self.call("PUT", &format!("/rest/config/devices/{}", encode(id)), &[], Some(json!({"deviceID":id,"name":name,"addresses":addresses,"autoAcceptFolders":false,"introducer":false})))?;
        Ok(())
    }
    /// 根据 spec 添加或替换文件夹，返回配置校验或管理接口错误。
    pub fn put_folder(&self, spec: &FolderSpec) -> Result<(), AppError> {
        self.call(
            "PUT",
            &format!("/rest/config/folders/{}", encode(&spec.id)),
            &[],
            Some(spec.to_config()?),
        )?;
        Ok(())
    }
    /// 用 patch 更新 id 文件夹；返回更新结果。路径与版本策略变更应使用经过校验的 FolderSpec。
    pub fn patch_folder(&self, id: &str, patch: Value) -> Result<(), AppError> {
        validate_task_folder_id(id)?;
        let object = patch.as_object().ok_or_else(|| {
            error(
                "syncthing.folder_patch_must_be_an_object",
                "文件夹补丁必须是对象",
            )
        })?;
        if object.keys().any(|key| {
            ![
                "paused",
                "fsWatcherDelayS",
                "rescanIntervalS",
                "label",
                "devices",
            ]
            .contains(&key.as_str())
        }) {
            return Err(AppError::new("syncthing.forbidden_change"));
        }
        if let Some(delay) = object.get("fsWatcherDelayS")
            && !delay.as_u64().is_some_and(|v| (1..=60).contains(&v))
        {
            return Err(error(
                "syncthing.sync_wait_time_must_be_1_60_seconds",
                "同步等待须在 1–60 秒内",
            ));
        }
        if let Some(devices) = object.get("devices") {
            for device in devices.as_array().ok_or_else(|| {
                error(
                    "syncthing.device_list_must_be_an_array",
                    "设备列表必须是数组",
                )
            })? {
                validate_device_id(
                    device["deviceID"]
                        .as_str()
                        .ok_or_else(|| error("syncthing.device_id_is_missing", "设备 ID 缺失"))?,
                )?;
            }
        }
        self.call(
            "PATCH",
            &format!("/rest/config/folders/{}", encode(id)),
            &[],
            Some(patch),
        )?;
        Ok(())
    }
    /// 将 lines 写入 folder 的忽略规则；先校验数量与每行长度。
    pub fn set_ignores(&self, folder: &str, lines: &[String]) -> Result<(), AppError> {
        validate_task_folder_id(folder)?;
        validate_ignores(lines)?;
        self.call(
            "POST",
            "/rest/db/ignores",
            &[("folder", folder)],
            Some(json!({"ignore":lines})),
        )?;
        Ok(())
    }
    /// 从 since 后长轮询 timeout_s 秒，types 为事件类型列表；返回事件及最后 ID（空结果沿用 since）。
    pub fn events(
        &self,
        since: u64,
        timeout_s: u32,
        types: &[String],
    ) -> Result<EventBatch, AppError> {
        let (timeout_s, timeout) = event_timeout(timeout_s);
        let value = self.request(
            "GET",
            "/rest/events",
            &[
                ("since", &since.to_string()),
                ("timeout", &timeout_s.to_string()),
                ("events", &types.join(",")),
            ],
            None,
            timeout,
        )?;
        let mut events = Vec::new();
        for value in value.as_array().ok_or_else(invalid)? {
            match serde_json::from_value::<Event>(value.clone()) {
                Ok(event) => events.push(event),
                Err(_) => eprintln!("跳过结构异常的同步事件"),
            }
        }
        let last_id = value
            .as_array()
            .ok_or_else(invalid)?
            .iter()
            .filter_map(|v| v["id"].as_u64())
            .max()
            .unwrap_or(since)
            .max(since);
        Ok(EventBatch { events, last_id })
    }
}
fn event_timeout(seconds: u32) -> (u32, Duration) {
    let seconds = seconds.clamp(1, 300);
    (seconds, Duration::from_secs(u64::from(seconds) + 10))
}
fn decode(status: u16, bytes: &[u8], secret: &str) -> Result<Value, AppError> {
    if bytes.len() > MAX_BODY {
        return Err(AppError::new("syncthing.response_too_large"));
    }
    if status == 403 {
        return Err(AppError::new("syncthing.unauthorized"));
    }
    if !(200..300).contains(&status) {
        return Err(AppError::new("syncthing.http_error")
            .with("status", status)
            .with(
                "body",
                redact(&String::from_utf8_lossy(bytes), &[secret])
                    .chars()
                    .take(512)
                    .collect::<String>(),
            ));
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(bytes).map_err(|_| invalid())
}

impl Client {
    /// 无参数；返回实例选项，其他字段保留为 JSON。
    pub fn get_options(&self) -> Result<Value, AppError> {
        self.call("GET", "/rest/config/options", &[], None)
    }
    /// 无参数；返回设备列表，其他字段保留为 JSON。
    pub fn list_devices(&self) -> Result<Value, AppError> {
        self.call("GET", "/rest/config/devices", &[], None)
    }
    /// 无参数；返回文件夹列表，其他字段保留为 JSON。
    pub fn list_folders(&self) -> Result<Value, AppError> {
        self.call("GET", "/rest/config/folders", &[], None)
    }
    /// 无参数；返回连接状态，其他字段保留为 JSON。
    pub fn connections(&self) -> Result<Value, AppError> {
        self.call("GET", "/rest/system/connections", &[], None)
    }
    /// 用 patch 更新实例选项；返回管理接口结果，仅允许重连间隔及收发限速。
    pub fn patch_options(&self, patch: Value) -> Result<(), AppError> {
        let object = patch.as_object().ok_or_else(|| {
            error(
                "syncthing.options_patch_must_be_an_object",
                "选项补丁必须是对象",
            )
        })?;
        if object.keys().any(|key| {
            !["reconnectionIntervalS", "maxSendKbps", "maxRecvKbps"].contains(&key.as_str())
        }) {
            return Err(AppError::new("syncthing.forbidden_change"));
        }
        self.call("PATCH", "/rest/config/options", &[], Some(patch))?;
        Ok(())
    }
    /// 无参数；请求正常关闭实例。
    pub fn shutdown(&self) -> Result<(), AppError> {
        self.shutdown_with_timeout(Duration::from_secs(30))
    }
    pub(crate) fn shutdown_with_timeout(&self, timeout: Duration) -> Result<(), AppError> {
        self.request("POST", "/rest/system/shutdown", &[], None, timeout)?;
        Ok(())
    }
    /// 根据 id 移除配置；不删除文件内容，返回操作结果。
    pub fn remove_device(&self, id: &str) -> Result<(), AppError> {
        validate_device_id(id)?;
        self.call(
            "DELETE",
            &format!("/rest/config/devices/{}", encode(id)),
            &[],
            None,
        )?;
        Ok(())
    }
    /// 根据 id 移除配置；不删除文件内容，返回操作结果。
    pub fn remove_folder(&self, id: &str) -> Result<(), AppError> {
        validate_task_folder_id(id)?;
        self.call(
            "DELETE",
            &format!("/rest/config/folders/{}", encode(id)),
            &[],
            None,
        )?;
        Ok(())
    }
    /// 根据 id 查询文件夹；不存在时返回 None。
    pub fn get_folder(&self, id: &str) -> Result<Option<Value>, AppError> {
        validate_task_folder_id(id)?;
        match self.call(
            "GET",
            &format!("/rest/config/folders/{}", encode(id)),
            &[],
            None,
        ) {
            Ok(v) => Ok(Some(v)),
            Err(e)
                if e.code == "syncthing.http_error"
                    && e.params.get("status").is_some_and(|v| v == "404") =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }
    /// 查询或操作 folder，附加参数按名称传给管理接口；返回原始 JSON 结果。
    pub fn get_ignores(&self, folder: &str) -> Result<Value, AppError> {
        validate_task_folder_id(folder)?;

        self.call("GET", "/rest/db/ignores", &[("folder", folder)], None)
    }
    /// 查询或操作 folder，附加参数按名称传给管理接口；返回原始 JSON 结果。
    pub fn db_status(&self, folder: &str) -> Result<Value, AppError> {
        validate_task_folder_id(folder)?;

        self.call("GET", "/rest/db/status", &[("folder", folder)], None)
    }
    /// 查询或操作 folder，附加参数按名称传给管理接口；返回原始 JSON 结果。
    pub fn db_completion(&self, folder: &str, device: &str) -> Result<Value, AppError> {
        validate_task_folder_id(folder)?;
        validate_device_id(device)?;
        self.call(
            "GET",
            "/rest/db/completion",
            &[("folder", folder), ("device", device)],
            None,
        )
    }
    /// 查询或操作 folder，附加参数按名称传给管理接口；返回原始 JSON 结果。
    pub fn scan(&self, folder: &str, sub: &str) -> Result<Value, AppError> {
        validate_task_folder_id(folder)?;

        self.call(
            "POST",
            "/rest/db/scan",
            &[("folder", folder), ("sub", sub)],
            None,
        )
    }
    /// 扫描整个 folder_id，使用 timeout 作为请求总时限；返回 Syncthing 原始响应或受控错误。
    pub fn scan_with_timeout(&self, folder_id: &str, timeout: Duration) -> Result<Value, AppError> {
        validate_task_folder_id(folder_id)?;
        if timeout.is_zero() {
            return Err(AppError::new("syncthing.timeout"));
        }
        self.request(
            "POST",
            "/rest/db/scan",
            &[("folder", folder_id)],
            None,
            timeout,
        )
    }
    /// 查询或操作 folder，附加参数按名称传给管理接口；返回原始 JSON 结果。
    pub fn browse(&self, folder: &str, prefix: &str, levels: u32) -> Result<Value, AppError> {
        validate_task_folder_id(folder)?;

        self.call(
            "GET",
            "/rest/db/browse",
            &[
                ("folder", folder),
                ("prefix", prefix),
                ("levels", &levels.to_string()),
            ],
            None,
        )
    }
    /// 查询或操作 folder，附加参数按名称传给管理接口；返回原始 JSON 结果。
    pub fn file_info(&self, folder: &str, path: &str) -> Result<Value, AppError> {
        validate_task_folder_id(folder)?;

        self.call(
            "GET",
            "/rest/db/file",
            &[("folder", folder), ("file", path)],
            None,
        )
    }
    /// 查询或操作 folder，附加参数按名称传给管理接口；返回原始 JSON 结果。
    pub fn folder_versions(&self, folder: &str) -> Result<Value, AppError> {
        validate_task_folder_id(folder)?;
        match self.call("GET", "/rest/folder/versions", &[("folder", folder)], None) {
            // 关闭了旧版本保留的文件夹：Syncthing 返回 500 “folder has no versioner”，按“没有旧版本”处理
            Err(e)
                if e.code == "syncthing.http_error"
                    && e.params
                        .get("body")
                        .is_some_and(|b| b.contains("no versioner")) =>
            {
                Ok(Value::Object(Default::default()))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn 监听补丁与事件边界() {
        let c = Client::new("http://127.0.0.1:1", "key").unwrap();
        for key in [
            "tcp://0.0.0.0:22000",
            "quic://127.0.0.1:22000",
            "tcp://[::]:22000",
        ] {
            let status = json!({"connectionServiceStatus": {key: {"lanAddresses":["x"]}}});
            assert_eq!(
                c.validate_listeners(&status).unwrap_err().code,
                "syncthing.listen_address_mismatch"
            );
        }
        assert!(c.validate_listeners(&json!({"connectionServiceStatus": {"tcp://127.0.0.1:22000": {"lanAddresses":["x"]}}})).unwrap());
        assert!(!c.validate_listeners(&json!({})).unwrap());
        for key in [
            "path",
            "versioning",
            "maxConflicts",
            "markerName",
            "type",
            "unknown",
        ] {
            assert_eq!(
                c.patch_folder("pl-1234567890123456", json!({key: "external"}))
                    .unwrap_err()
                    .code,
                "syncthing.forbidden_change"
            );
        }
        for key in [
            "listenAddresses",
            "globalAnnounceEnabled",
            "relaysEnabled",
            "natEnabled",
            "unknown",
        ] {
            assert_eq!(
                c.patch_options(json!({key: true})).unwrap_err().code,
                "syncthing.forbidden_change"
            );
        }
        for (input, expected) in [(0, 1), (1, 1), (300, 300), (301, 300), (u32::MAX, 300)] {
            assert_eq!(
                event_timeout(input),
                (expected, Duration::from_secs(u64::from(expected) + 10))
            );
        }
    }
    #[test]
    fn l10_地址认证头及编码限制() {
        assert!(Client::new("http://127.0.0.1:8384", "密钥").is_ok());
        for value in [
            "http://localhost:8384",
            "https://127.0.0.1:8384",
            "http://127.0.0.1:0",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:1/path",
            "http://127.0.0.1:1@evil",
            "http://127.0.0.1:1?x",
            "http://127.0.0.1:1/",
            "http://[::1]:8384",
        ] {
            assert!(Client::new(value, "key").is_err(), "{value}");
        }
        assert!(Client::new("http://127.0.0.1:1", "x\r\nHost:evil").is_err());
        assert_eq!(
            encode("a/中文 &?=#"),
            "a%2F%E4%B8%AD%E6%96%87%20%26%3F%3D%23"
        );
    }
    #[test]
    fn 响应上限错误码脱敏和事件解析() {
        assert_eq!(
            decode(200, b"{\"ping\":\"pong\"}", "key").unwrap()["ping"],
            "pong"
        );
        assert_eq!(decode(204, b"", "key").unwrap(), Value::Null);
        assert_eq!(
            decode(403, b"", "key").unwrap_err().code,
            "syncthing.unauthorized"
        );
        assert_eq!(
            decode(200, b"invalid", "key").unwrap_err().code,
            "syncthing.invalid_response"
        );
        assert_eq!(
            decode(200, &vec![b'x'; MAX_BODY + 1], "key")
                .unwrap_err()
                .code,
            "syncthing.response_too_large"
        );
        let e = decode(
            500,
            format!("key password=秘密\n{}", "中".repeat(600)).as_bytes(),
            "key",
        )
        .unwrap_err();
        assert_eq!(e.code, "syncthing.http_error");
        assert_eq!(e.params["status"], "500");
        assert!(!format!("{e:?}").contains("秘密"));
        assert!(!e.params["body"].contains("key"));
        assert_eq!(e.params["body"].chars().count(), 512);
        let events: Vec<Event> = serde_json::from_value(json!([{"id":9,"type":"StateChanged","time":"2026-10-22T00:00:00Z","data":{"unknown":true},"extra":1}])).unwrap();
        assert_eq!(events[0].id, 9);
        assert_eq!(events[0].data["unknown"], true);
    }
    #[test]
    fn 写请求在联网前校验() {
        let c = Client::new("http://127.0.0.1:1", "key").unwrap();
        let id = "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA";
        assert!(c.add_device(id, "x", &["dynamicx".into()]).is_err());
        assert!(c.add_device(id, "x", &[]).is_err());
        assert!(
            c.add_device("无效", "x", &["tcp://127.0.0.1:1234".into()])
                .is_err()
        );
        assert!(c.remove_device("无效").is_err());
        assert!(c.remove_folder("无效").is_err());
        assert!(c.get_folder("无效").is_err());
        assert!(c.put_folder(&FolderSpec::default()).is_err());
        assert!(
            c.patch_folder("pl-1234567890123456", json!({"fsWatcherDelayS":0}))
                .is_err()
        );
        assert!(c.patch_options(json!(null)).is_err());
        assert!(
            c.set_ignores("pl-1234567890123456", &["\0".into()])
                .is_err()
        );
        assert!(c.db_completion("pl-1234567890123456", "无效").is_err());
        for result in [
            c.get_ignores("无效"),
            c.db_status("无效"),
            c.scan("无效", ""),
            c.browse("无效", "", 0),
            c.file_info("无效", "x"),
            c.folder_versions("无效"),
        ] {
            assert!(result.is_err());
        }
    }
}

#[cfg(all(test, unix))]
mod scan_timeout_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Instant,
    };
    #[test]
    #[ignore = "需要允许本机随机监听端口"]
    fn 扫描请求遵守调用方时限并可放宽时限() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = Client::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            "测试凭据",
        )
        .unwrap();
        let peer = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 4096];
                let count = stream.read(&mut request).unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..count])
                        .starts_with("POST /rest/db/scan?folder=pl-1234567890123456 ")
                );
                thread::sleep(Duration::from_millis(150));
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
            }
        });
        let started = Instant::now();
        assert!(
            client
                .scan_with_timeout("pl-1234567890123456", Duration::from_millis(30))
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_millis(140));
        assert!(
            client
                .scan_with_timeout("pl-1234567890123456", Duration::from_secs(2))
                .is_ok()
        );
        assert!(
            client
                .scan_with_timeout("无效", Duration::from_secs(1))
                .is_err()
        );
        peer.join().unwrap();
    }
}
