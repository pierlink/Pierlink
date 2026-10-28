//! 管理页配置核对；期望值由任务数据库生成，秘密只用于恢复，不进入对比结果。
use crate::{
    error::AppError,
    syncthing::{
        client::Client,
        folder::{validate_device_id, validate_ignores, validate_task_folder_id},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path, time::Duration};

/// 从实例创建时保存的可信配置提供加固值；password_hash 是已有 bcrypt 摘要，不能传明文。
/// 本机通过环境变量提供 API 密钥时，api_key 应为配置文件中的空字符串。
pub struct HardeningPolicy {
    pub gui_address: String,
    pub gui_user: String,
    pub password_hash: String,
    pub api_key: String,
    pub listen_port: u16,
}
/// 在 init_home/LocalSync::start 成功后、打开管理页之前，按 spec 取得并保存可信加固基线。
/// 返回值只保存在后端内存；不能在核对阶段重新采集，否则会把管理页修改过的凭据当作期望值。
pub fn capture_hardening_policy(
    client: &Client,
    spec: &crate::syncthing::home::HomeSpec,
) -> Result<HardeningPolicy, AppError> {
    let gui = call(client, "GET", "/rest/config/gui", None)?;
    let policy = HardeningPolicy {
        gui_address: spec.gui.address()?,
        gui_user: spec.gui_user.clone(),
        password_hash: gui["password"].as_str().ok_or_else(invalid)?.into(),
        api_key: gui["apiKey"].as_str().ok_or_else(invalid)?.into(),
        listen_port: spec.listen_port,
    };
    validate_policy(&policy)?;
    Ok(policy)
}
/// config 使用 FolderSpec::to_config() 或已保存的任务配置，忽略规则按原有顺序保存。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedFolder {
    pub config: Value,
    pub ignore_rules: Vec<String>,
}
/// 只填写 Pierlink 拥有的对象；设备包括本机设备，以免把本机误列为外来项。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManagedConfiguration {
    pub folders: Vec<ManagedFolder>,
    pub devices: Vec<Value>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigTarget {
    Folder,
    Device,
    Ignores,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigurationChange {
    pub target: ConfigTarget,
    pub id: String,
    /// 对象被删除时为 exists，其余为 REST 字段名；忽略规则为 ignore。
    pub field: String,
    pub expected: Value,
    pub actual: Value,
}
#[derive(Debug, Clone, Serialize)]
pub struct ForeignConfiguration {
    pub target: ConfigTarget,
    pub id: String,
    pub name: String,
    pub path: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConfigurationReport {
    /// 只返回字段位置，绝不包含 GUI 凭据及其摘要。
    pub hardened: Vec<String>,
    pub task_changes: Vec<ConfigurationChange>,
    pub foreign: Vec<ForeignConfiguration>,
}
/// 执行人把该字段写进任务设置；needs_rejoin 时先走路径预检及重新加入流程，再更新数据库。
#[derive(Debug, Clone, Serialize)]
pub struct AdoptedConfiguration {
    pub change: ConfigurationChange,
    pub needs_rejoin: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct RestoredConfiguration {
    pub paused_for_rejoin: bool,
}

const FOLDER_FIELDS: &[&str] = &[
    "label",
    "path",
    "type",
    "devices",
    "ignorePerms",
    "versioning",
    "rescanIntervalS",
    "fsWatcherEnabled",
    "fsWatcherDelayS",
    "paused",
    "markerName",
];
const DEVICE_FIELDS: &[&str] = &[
    "name",
    "addresses",
    "compression",
    "paused",
    "introducer",
    "autoAcceptFolders",
];
fn invalid() -> AppError {
    AppError::new("sync.invalid_configuration")
}
fn call(client: &Client, method: &str, path: &str, body: Option<Value>) -> Result<Value, AppError> {
    client.call(method, path, &[], body).map_err(|mut e| {
        e.params.remove("body");
        e
    })
}
fn section(value: &Value, key: &str) -> Result<Vec<Value>, AppError> {
    value[key].as_array().cloned().ok_or_else(invalid)
}
fn validate(expected: &ManagedConfiguration) -> Result<(), AppError> {
    let mut ids = HashSet::new();
    for folder in &expected.folders {
        let id = folder.config["id"].as_str().ok_or_else(invalid)?;
        validate_task_folder_id(id)?;
        if !ids.insert(id) {
            return Err(invalid());
        }
        validate_ignores(&folder.ignore_rules)?;
        if folder.config["path"].as_str().is_none() {
            return Err(invalid());
        }
        // 只校验设备编号；对比结果另行剔除加密密码等扩展字段。
        if let Some(devices) = folder.config["devices"].as_array() {
            for device in devices {
                validate_device_id(device["deviceID"].as_str().ok_or_else(invalid)?)?;
            }
        }
    }
    ids.clear();
    for device in &expected.devices {
        let id = device["deviceID"].as_str().ok_or_else(invalid)?;
        validate_device_id(id)?;
        if !ids.insert(id) {
            return Err(invalid());
        }
    }
    Ok(())
}
fn projected(field: &str, value: &Value) -> Value {
    if field == "devices" {
        let Some(devices) = value.as_array() else {
            return value.clone();
        };
        let mut ids: Vec<_> = devices
            .iter()
            .filter_map(|d| d["deviceID"].as_str())
            .collect();
        ids.sort_unstable();
        return json!(
            ids.into_iter()
                .map(|id| json!({"deviceID":id}))
                .collect::<Vec<_>>()
        );
    }
    if field == "versioning" {
        // 仅输出任务拥有的旧版本策略，不透传未知扩展字段。
        return json!({"type":value["type"].as_str().unwrap_or(""), "params": if value["params"]["cleanoutDays"].is_null() { json!({}) } else { json!({"cleanoutDays":value["params"]["cleanoutDays"]}) }, "fsPath":value["fsPath"].as_str().unwrap_or(""), "fsType":value["fsType"].as_str().unwrap_or("basic")});
    }
    value.clone()
}
fn changes(
    target: ConfigTarget,
    id: &str,
    expected: &Value,
    current: Option<&Value>,
    fields: &[&str],
    report: &mut ConfigurationReport,
) {
    let Some(current) = current else {
        report.task_changes.push(ConfigurationChange {
            target,
            id: id.into(),
            field: "exists".into(),
            expected: json!(true),
            actual: json!(false),
        });
        return;
    };
    for &field in fields {
        if expected.get(field).is_none() {
            continue;
        }
        let a = projected(field, &expected[field]);
        let b = projected(field, &current[field]);
        if a != b {
            report.task_changes.push(ConfigurationChange {
                target,
                id: id.into(),
                field: field.into(),
                expected: a,
                actual: b,
            });
        }
    }
}
fn task_report(
    client: &Client,
    expected: &ManagedConfiguration,
    current: &Value,
) -> Result<ConfigurationReport, AppError> {
    let folders = section(current, "folders")?;
    let devices = section(current, "devices")?;
    let mut report = ConfigurationReport::default();
    for folder in &expected.folders {
        let id = folder.config["id"].as_str().ok_or_else(invalid)?;
        let actual = folders.iter().find(|f| f["id"] == id);
        changes(
            ConfigTarget::Folder,
            id,
            &folder.config,
            actual,
            FOLDER_FIELDS,
            &mut report,
        );
        if actual.is_some() {
            let value = client.get_ignores(id).map_err(|mut e| {
                e.params.remove("body");
                e
            })?;
            let actual = value["ignore"].as_array().cloned().ok_or_else(invalid)?;
            if json!(actual) != json!(folder.ignore_rules) {
                report.task_changes.push(ConfigurationChange {
                    target: ConfigTarget::Ignores,
                    id: id.into(),
                    field: "ignore".into(),
                    expected: json!(folder.ignore_rules),
                    actual: json!(actual),
                });
            }
        }
    }
    for device in &expected.devices {
        let id = device["deviceID"].as_str().ok_or_else(invalid)?;
        changes(
            ConfigTarget::Device,
            id,
            device,
            devices.iter().find(|d| d["deviceID"] == id),
            DEVICE_FIELDS,
            &mut report,
        );
    }
    for (target, objects, id_key, name_key) in [
        (ConfigTarget::Folder, &folders, "id", "label"),
        (ConfigTarget::Device, &devices, "deviceID", "name"),
    ] {
        for object in objects {
            let id = object[id_key].as_str().ok_or_else(invalid)?;
            let managed = match target {
                ConfigTarget::Folder => expected.folders.iter().any(|f| f.config["id"] == id),
                _ => expected.devices.iter().any(|d| d["deviceID"] == id),
            };
            if !managed {
                report.foreign.push(ForeignConfiguration {
                    target,
                    id: id.into(),
                    name: object[name_key].as_str().unwrap_or("").into(),
                    path: object["path"].as_str().map(String::from),
                });
            }
        }
    }
    Ok(report)
}
fn validate_policy(policy: &HardeningPolicy) -> Result<(), AppError> {
    crate::syncthing::client::loopback_port(&policy.gui_address, "127.0.0.1:")?;
    if policy.listen_port == 0
        || policy.gui_user.is_empty()
        || policy.password_hash.len() != 60
        || !["$2a$", "$2b$", "$2y$"]
            .iter()
            .any(|p| policy.password_hash.starts_with(p))
    {
        return Err(invalid());
    }
    Ok(())
}
fn harden(
    client: &Client,
    policy: &HardeningPolicy,
    expected: &ManagedConfiguration,
    current: &mut Value,
) -> Result<Vec<String>, AppError> {
    validate_policy(policy)?;
    let gui = json!({"address":policy.gui_address,"user":policy.gui_user,"password":policy.password_hash,"apiKey":policy.api_key,"enabled":true,"useTLS":false,"authMode":"static","metricsWithoutAuth":false,"insecureSkipHostcheck":false,"insecureAdminAccess":false});
    let options = json!({"listenAddresses":[format!("tcp://127.0.0.1:{}",policy.listen_port)],"globalAnnounceEnabled":false,"localAnnounceEnabled":false,"relaysEnabled":false,"natEnabled":false,"crashReportingEnabled":false,"startBrowser":false,"announceLANAddresses":false,"autoUpgradeIntervalH":0,"urAccepted":-1,"urSeen":2147483647});
    let defaults = json!({"maxConflicts":-1});
    let mut restored = Vec::new();
    for folder in current["folders"].as_array_mut().ok_or_else(invalid)? {
        if expected
            .folders
            .iter()
            .any(|f| f.config["id"] == folder["id"])
            && folder["maxConflicts"] != -1
        {
            let id = folder["id"].as_str().ok_or_else(invalid)?;
            call(
                client,
                "PATCH",
                &format!("/rest/config/folders/{id}"),
                Some(json!({"maxConflicts":-1})),
            )?;
            restored.push(format!("/folders/{id}/maxConflicts"));
            folder["maxConflicts"] = json!(-1);
        }
    }
    // GUI 最后写：变更监听地址可能中断当前管理连接。
    for (pointer, endpoint, wanted) in [
        ("/options", "options", options),
        ("/defaults/folder", "defaults/folder", defaults),
        ("/gui", "gui", gui),
    ] {
        let actual = current.pointer_mut(pointer).ok_or_else(invalid)?;
        if !actual.is_object() {
            return Err(invalid());
        }
        let mut patch = serde_json::Map::new();
        for (key, value) in wanted.as_object().ok_or_else(invalid)? {
            if actual[key] != *value {
                patch.insert(key.clone(), value.clone());
            }
        }
        if !patch.is_empty() {
            call(
                client,
                "PATCH",
                &format!("/rest/config/{endpoint}"),
                Some(Value::Object(patch.clone())),
            )?;
            for (key, value) in patch {
                actual[&key] = value;
                restored.push(format!("{pointer}/{key}"));
            }
        }
    }
    Ok(restored)
}
/// 核对 client 当前配置与 expected，按 policy 静默恢复加固项，返回任务差异及外来对象。
/// 可在启动时调用；阻塞网络操作。policy 必须来自可信初始配置，不能从待核对的 GUI 配置重建。
pub fn reconcile_configuration(
    client: &Client,
    policy: &HardeningPolicy,
    expected: &ManagedConfiguration,
) -> Result<ConfigurationReport, AppError> {
    validate(expected)?;
    let mut current = call(client, "GET", "/rest/config", None)?;
    let task_result = task_report(client, expected, &current);
    let hardened = harden(client, policy, expected, &mut current)?;
    let mut report = task_result?;
    report.hardened = hardened;
    Ok(report)
}
fn fresh(
    client: &Client,
    expected: &ManagedConfiguration,
    change: &ConfigurationChange,
) -> Result<(), AppError> {
    validate(expected)?;
    let current = call(client, "GET", "/rest/config", None)?;
    if !task_report(client, expected, &current)?
        .task_changes
        .contains(change)
    {
        return Err(AppError::new("sync.configuration_changed"));
    }
    Ok(())
}
fn validate_adoption(change: &ConfigurationChange) -> Result<(), AppError> {
    let value = &change.actual;
    let valid = match change.field.as_str() {
        "exists" => value == false,
        "path" => value
            .as_str()
            .is_some_and(|p| Path::new(p).is_absolute() && !p.contains('\0')),
        "type" => value
            .as_str()
            .is_some_and(|v| ["sendreceive", "receiveonly"].contains(&v)),
        "fsWatcherDelayS" => value
            .as_f64()
            .is_some_and(|v| (1.0..=60.0).contains(&v) && v.fract() == 0.0),
        "rescanIntervalS" => value.as_u64().is_some_and(|v| v <= u32::MAX as u64),
        "ignore" => {
            let rules: Vec<String> =
                serde_json::from_value(value.clone()).map_err(|_| invalid())?;
            validate_ignores(&rules)?;
            true
        }
        "devices" => {
            for d in value.as_array().ok_or_else(invalid)? {
                validate_device_id(d["deviceID"].as_str().ok_or_else(invalid)?)?;
            }
            true
        }
        "addresses" => {
            for address in value.as_array().ok_or_else(invalid)? {
                crate::syncthing::client::loopback_port(
                    address.as_str().ok_or_else(invalid)?,
                    "tcp://127.0.0.1:",
                )?;
            }
            !value.as_array().is_none_or(Vec::is_empty)
        }
        "versioning" => {
            value["type"] == ""
                || (value["type"] == "trashcan"
                    && value["params"]["cleanoutDays"]
                        .as_str()
                        .and_then(|s| s.parse::<u32>().ok())
                        .is_some_and(|d| d > 0))
        }
        "ignorePerms" | "fsWatcherEnabled" | "paused" => value.is_boolean(),
        "introducer" | "autoAcceptFolders" => value == false,
        "markerName" => value.as_str().is_some_and(|name| {
            !name.is_empty()
                && name != "."
                && name != ".."
                && !name.contains(['/', '\\', '\0', ':'])
        }),
        "compression" => value
            .as_str()
            .is_some_and(|v| ["always", "metadata", "never"].contains(&v)),
        "label" | "name" => value.is_string(),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(AppError::new("sync.configuration_not_adoptable").with("field", &change.field))
    }
}
/// 重新核对 change 未过期，返回需保存的单字段新值；不写数据库。
/// 路径及删除任务的采用须由调用方先完成预检/移除/重新加入；不能直接在旧索引上换路径。
pub fn adopt_configuration_change(
    client: &Client,
    expected: &ManagedConfiguration,
    change: &ConfigurationChange,
) -> Result<AdoptedConfiguration, AppError> {
    fresh(client, expected, change)?;
    validate_adoption(change)?;
    if change.field == "versioning" && change.actual["type"] != "" {
        let path = change.actual["fsPath"].as_str().ok_or_else(invalid)?;
        let versions = crate::syncthing::folder::normalized(Path::new(path))?;
        let current = client
            .get_folder(&change.id)
            .map_err(|mut e| {
                e.params.remove("body");
                e
            })?
            .ok_or_else(invalid)?;
        let folder = crate::syncthing::folder::normalized(Path::new(
            current["path"].as_str().ok_or_else(invalid)?,
        ))?;
        if change.actual["fsType"] != "basic"
            || crate::storage::paths_overlap(&folder.to_string_lossy(), &versions.to_string_lossy())
        {
            return Err(AppError::new("sync.configuration_not_adoptable")
                .with("field", "versioning")
                .with("reason", "旧版本目录必须使用项目外的本机目录"));
        }
    }
    Ok(AdoptedConfiguration {
        change: change.clone(),
        needs_rejoin: change.field == "path" || change.field == "exists",
    })
}
/// 重新核对 change 后只恢复所选字段，保留其他改动和外来对象；路径恢复会清索引并暂停重新加入。
/// 对象被删除时恢复完整的期望对象（文件夹保持暂停），返回是否需要用户确认后继续同步。
pub fn restore_configuration_change(
    client: &Client,
    expected: &ManagedConfiguration,
    change: &ConfigurationChange,
) -> Result<RestoredConfiguration, AppError> {
    restore_change(client, expected, change).map_err(|mut error| {
        error.params.remove("body");
        error
    })
}
fn restore_change(
    client: &Client,
    expected: &ManagedConfiguration,
    change: &ConfigurationChange,
) -> Result<RestoredConfiguration, AppError> {
    fresh(client, expected, change)?;
    let id = &change.id;
    let mut paused = false;
    match change.target {
        ConfigTarget::Ignores => {
            let rules: Vec<String> =
                serde_json::from_value(change.expected.clone()).map_err(|_| invalid())?;
            client.set_ignores(id, &rules).map_err(|mut e| {
                e.params.remove("body");
                e
            })?;
        }
        ConfigTarget::Folder if change.field == "path" => {
            super::tasks::relocate(
                client,
                id,
                Path::new(change.expected.as_str().ok_or_else(invalid)?),
                Duration::from_secs(30),
            )?;
            paused = true;
        }
        target => {
            let section = if target == ConfigTarget::Folder {
                "folders"
            } else {
                "devices"
            };
            if change.field == "exists" {
                let mut value = if target == ConfigTarget::Folder {
                    expected
                        .folders
                        .iter()
                        .find(|f| f.config["id"] == *id)
                        .ok_or_else(invalid)?
                        .config
                        .clone()
                } else {
                    expected
                        .devices
                        .iter()
                        .find(|d| d["deviceID"] == *id)
                        .ok_or_else(invalid)?
                        .clone()
                };
                if target == ConfigTarget::Folder {
                    super::tasks::wait_index_clear(client, id, Duration::from_secs(30))?;
                    value["paused"] = json!(true);
                    value["maxConflicts"] = json!(-1);
                    paused = true;
                }
                call(
                    client,
                    "PUT",
                    &format!("/rest/config/{section}/{id}"),
                    Some(value),
                )?;
                if target == ConfigTarget::Folder {
                    let folder = expected
                        .folders
                        .iter()
                        .find(|f| f.config["id"] == *id)
                        .ok_or_else(invalid)?;
                    client.set_ignores(id, &folder.ignore_rules)?;
                }
            } else {
                call(
                    client,
                    "PATCH",
                    &format!("/rest/config/{section}/{id}"),
                    Some(json!({&change.field:change.expected})),
                )?;
            }
        }
    }
    Ok(RestoredConfiguration {
        paused_for_rejoin: paused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    const ID: &str = "pl-1234567890123456";
    const DEVICE: &str = "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA";
    struct State {
        config: Value,
        ignores: Vec<String>,
        writes: Vec<String>,
    }
    fn fixture() -> (
        Client,
        Arc<Mutex<State>>,
        ManagedConfiguration,
        HardeningPolicy,
    ) {
        let folder = json!({"id":ID,"label":"任务","path":"/work","type":"sendreceive","devices":[{"deviceID":DEVICE}],"ignorePerms":true,"versioning":{"type":"","params":{}},"rescanIntervalS":3600,"fsWatcherEnabled":true,"fsWatcherDelayS":10,"paused":true,"markerName":".stfolder","maxConflicts":-1});
        let device = json!({"deviceID":DEVICE,"name":"服务器","addresses":["tcp://127.0.0.1:1234"],"introducer":false,"autoAcceptFolders":false});
        let expected = ManagedConfiguration {
            folders: vec![ManagedFolder {
                config: folder.clone(),
                ignore_rules: vec!["(?d).git".into()],
            }],
            devices: vec![device.clone()],
        };
        let state = Arc::new(Mutex::new(State {
            config: json!({"gui":{},"options":{},"defaults":{"folder":{}},"folders":[folder,{"id":"foreign","path":"/foreign","label":"外来","maxConflicts":7}],"devices":[device,{"deviceID":"foreign","name":"外来"}]}),
            ignores: vec!["changed".into()],
            writes: vec![],
        }));
        let shared = state.clone();
        let mut client = Client::new("http://127.0.0.1:1234", "secret").unwrap();
        client.test_handler = Some(Arc::new(move |method, path, _, body| {
            let mut s = shared.lock().unwrap();
            match (method, path) {
                ("GET", "/rest/config") => Ok(s.config.clone()),
                ("GET", "/rest/config/gui") => Ok(s.config["gui"].clone()),
                ("GET", "/rest/db/status") => {
                    Ok(json!({"localFiles":0,"localDeleted":0,"localDirectories":0}))
                }
                ("GET", "/rest/db/ignores") => Ok(json!({"ignore":s.ignores})),
                ("POST", "/rest/db/ignores") => {
                    s.ignores = serde_json::from_value(body.unwrap()["ignore"].clone()).unwrap();
                    s.writes.push(path.into());
                    Ok(Value::Null)
                }
                ("PATCH", _) => {
                    let pointer = path.strip_prefix("/rest/config").unwrap();
                    let target = if pointer.starts_with("/folders/") {
                        &mut s.config["folders"][0]
                    } else if pointer.starts_with("/devices/") {
                        &mut s.config["devices"][0]
                    } else {
                        s.config.pointer_mut(pointer).unwrap()
                    };
                    target
                        .as_object_mut()
                        .unwrap()
                        .extend(body.unwrap().as_object().unwrap().clone());
                    s.writes.push(path.into());
                    Ok(Value::Null)
                }
                ("PUT", _) => {
                    let section = if path.contains("/folders/") {
                        "folders"
                    } else {
                        "devices"
                    };
                    s.config[section]
                        .as_array_mut()
                        .unwrap()
                        .push(body.unwrap());
                    s.writes.push(path.into());
                    Ok(Value::Null)
                }
                _ => panic!("未处理请求 {method} {path}"),
            }
        }));
        let policy = HardeningPolicy {
            gui_address: "127.0.0.1:1234".into(),
            gui_user: "user".into(),
            password_hash: format!("$2b$12${}", "x".repeat(53)),
            api_key: String::new(),
            listen_port: 1235,
        };
        (client, state, expected, policy)
    }
    #[test]
    fn 三类配置加固恢复与逐项采用不覆盖外来项() {
        let (client, state, expected, policy) = fixture();
        state.lock().unwrap().config["folders"][0]["fsWatcherDelayS"] = json!(3);
        state.lock().unwrap().config["folders"][0]["maxConflicts"] = json!(0);
        let report = reconcile_configuration(&client, &policy, &expected).unwrap();
        let root = crate::test_support::tempdir();
        let spec = crate::syncthing::home::HomeSpec {
            home: root.path().into(),
            gui: crate::syncthing::home::GuiListen::Tcp { port: 1234 },
            gui_user: "user".into(),
            listen_port: 1235,
        };
        let captured = capture_hardening_policy(&client, &spec).unwrap();
        assert!(captured.password_hash == policy.password_hash);
        assert!(captured.api_key == policy.api_key);
        assert!(report.hardened.contains(&"/gui/password".into()));
        assert!(
            report
                .hardened
                .contains(&format!("/folders/{ID}/maxConflicts"))
        );
        assert_eq!(report.foreign.len(), 2);
        assert_eq!(report.task_changes.len(), 2);
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains(&policy.password_hash)
        );
        let delay = report
            .task_changes
            .iter()
            .find(|c| c.field == "fsWatcherDelayS")
            .unwrap();
        let adopted = adopt_configuration_change(&client, &expected, delay).unwrap();
        assert_eq!(adopted.change.actual, 3);
        assert!(!adopted.needs_rejoin);
        let ignores = report
            .task_changes
            .iter()
            .find(|c| c.target == ConfigTarget::Ignores)
            .unwrap();
        restore_configuration_change(&client, &expected, ignores).unwrap();
        assert_eq!(
            state.lock().unwrap().ignores,
            expected.folders[0].ignore_rules
        );
        assert_eq!(
            state.lock().unwrap().config["folders"][0]["fsWatcherDelayS"],
            3
        );
        restore_configuration_change(&client, &expected, delay).unwrap();
        let report = reconcile_configuration(&client, &policy, &expected).unwrap();
        assert!(report.hardened.is_empty() && report.task_changes.is_empty());
        assert_eq!(
            state.lock().unwrap().config["folders"][1]["maxConflicts"],
            7
        );
        assert_eq!(
            restore_configuration_change(&client, &expected, delay)
                .unwrap_err()
                .code,
            "sync.configuration_changed"
        );
    }
    #[test]
    fn 缺失对象路径采用与过期差异拒绝() {
        let (client, state, expected, policy) = fixture();
        // 采用的目录必须是本机的绝对路径：Windows 上 /new 不算
        let new_path = if cfg!(windows) { r"C:\new" } else { "/new" };
        state.lock().unwrap().config["folders"][0]["path"] = json!(new_path);
        let report = reconcile_configuration(&client, &policy, &expected).unwrap();
        let path = report
            .task_changes
            .iter()
            .find(|c| c.field == "path")
            .unwrap();
        assert!(
            adopt_configuration_change(&client, &expected, path)
                .unwrap()
                .needs_rejoin
        );
        let mut fake = path.clone();
        fake.actual = json!("/tampered");
        assert_eq!(
            adopt_configuration_change(&client, &expected, &fake)
                .unwrap_err()
                .code,
            "sync.configuration_changed"
        );
        state.lock().unwrap().config["folders"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        let report = reconcile_configuration(&client, &policy, &expected).unwrap();
        let missing = report
            .task_changes
            .iter()
            .find(|c| c.field == "exists")
            .unwrap();
        assert!(
            adopt_configuration_change(&client, &expected, missing)
                .unwrap()
                .needs_rejoin
        );
        assert!(
            restore_configuration_change(&client, &expected, missing)
                .unwrap()
                .paused_for_rejoin
        );
        let s = state.lock().unwrap();
        assert!(
            s.config["folders"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["id"] == ID && f["paused"] == true)
        );
    }
    #[test]
    fn 任务字段覆盖编码归一与禁止采用危险值() {
        let (client, state, expected, policy) = fixture();
        let cases = [
            ("type", json!("receiveonly")),
            ("devices", json!([])),
            ("ignorePerms", json!(false)),
            (
                "versioning",
                json!({"type":"trashcan","params":{"cleanoutDays":"30"},"fsPath":"/versions"}),
            ),
            ("rescanIntervalS", json!(60)),
            ("fsWatcherEnabled", json!(false)),
        ];
        for (field, value) in cases {
            state.lock().unwrap().config["folders"][0][field] = value;
        }
        state.lock().unwrap().config["devices"][0]["name"] = json!("新名称");
        let report = reconcile_configuration(&client, &policy, &expected).unwrap();
        assert_eq!(report.task_changes.len(), 8);
        for change in report.task_changes {
            restore_configuration_change(&client, &expected, &change).unwrap();
        }
        assert!(
            reconcile_configuration(&client, &policy, &expected)
                .unwrap()
                .task_changes
                .is_empty()
        );
        for (field, value) in [
            ("type", json!("sendonly")),
            ("fsWatcherDelayS", json!(0)),
            ("addresses", json!(["tcp://evil:22000"])),
            ("introducer", json!(true)),
        ] {
            let change = ConfigurationChange {
                target: ConfigTarget::Folder,
                id: ID.into(),
                field: field.into(),
                expected: Value::Null,
                actual: value,
            };
            assert!(validate_adoption(&change).is_err());
        }
    }
}
