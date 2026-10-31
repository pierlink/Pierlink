use super::*;
use crate::syncthing::{client::Client, folder::FolderKind, process::SyncthingBin};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};
const A: &str = "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA";
const B: &str = "BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB-BBBBBBB";
const ID: &str = "pl-1234567890123456";

#[test]
fn y8_y10_y24_d13_y22_本机路径() {
    let env = LocalEnv {
        home: "C:/Users/A".into(),
        local_app_data: "C:/Users/A/AppData/Local".into(),
        windows_dir: "C:/Windows".into(),
        onedrive: vec!["D:/Cloud".into()],
        ..Default::default()
    };
    for p in [
        "c:/",
        "C:/Users/A/",
        "c:/WINDOWS/a/..",
        "C:/Program Files/a",
        r"\\?\C:\Users\A\AppData\Local\Pierlink",
        "C:/Users/A/.ssh/a",
    ] {
        assert!(
            check_local_path(Path::new(p), &[], &env)
                .iter()
                .any(|p| matches!(p, PathIssue::Dangerous { .. })),
            "{p}"
        );
    }
    for p in [
        "D:/Cloud/work",
        "D:/Dropbox/x",
        "D:/OneDrive - company/x",
        r"\\server\share\x",
    ] {
        assert!(
            check_local_path(Path::new(p), &[], &env)
                .iter()
                .any(|p| matches!(p, PathIssue::CloudFolder { .. })),
            "{p}"
        );
    }
    let existing = vec![ExistingTask {
        task_id: "task".into(),
        local_path: "D:/a/b".into(),
    }];
    assert!(
        check_local_path(Path::new("d:/a/b/c"), &existing, &env)
            .iter()
            .any(|p| matches!(p, PathIssue::Overlap { .. }))
    );
    assert!(
        !check_local_path(Path::new("d:/a/bc"), &existing, &env)
            .iter()
            .any(|p| matches!(p, PathIssue::Overlap { .. }))
    );
    let dir = crate::test_support::tempdir();
    let path = dir.path().join("work");
    fs::create_dir(&path).unwrap();
    fs::create_dir(path.join(".stfolder")).unwrap();
    fs::write(path.join(".stignore"), "").unwrap();
    fs::create_dir(path.join(".git")).unwrap();
    let mut env = LocalEnv {
        home: dir.path().join("home"),
        local_app_data: dir.path().join("data"),
        windows_dir: dir.path().join("windows"),
        ..Default::default()
    };
    let issues = check_local_path(&path, &[], &env);
    assert!(issues.contains(&PathIssue::OtherSyncthing {
        side: "local".into()
    }));
    assert!(issues.contains(&PathIssue::IgnoreFile {
        side: "local".into()
    }));
    assert!(issues.contains(&PathIssue::GitRepository {
        side: "local".into()
    }));
    env.managed_paths.push(path.clone());
    assert!(
        !check_local_path(&path, &[], &env)
            .iter()
            .any(|p| matches!(p, PathIssue::OtherSyncthing { .. }))
    );
    #[cfg(unix)]
    {
        fs::create_dir_all(env.home.join(".ssh")).unwrap();
        std::os::unix::fs::symlink(env.home.join(".ssh"), dir.path().join("alias")).unwrap();
        assert!(
            check_local_path(&dir.path().join("alias/sub/.."), &[], &env)
                .iter()
                .any(|p| matches!(p, PathIssue::Dangerous { .. }))
        );
    }
}

#[cfg(unix)]
mod mock {
    const C: &str = "CCCCCCC-CCCCCCC-CCCCCCC-CCCCCCC-CCCCCCC-CCCCCCC-CCCCCCC-CCCCCCC";
    use super::*;
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct State {
        folders: Vec<Value>,
        devices: Vec<Value>,
        rules: Vec<String>,
        ignores_response: Option<Value>,
        requests: Vec<(String, String, Value)>,
        fail: Option<String>,
        index_reads: usize,
        files: bool,
        writes: usize,
        fail_writes_at: Vec<usize>,
        receive_only_items: u64,
    }
    struct Api {
        client: Client,
        state: Arc<Mutex<State>>,
    }
    impl Api {
        fn new(id: &'static str) -> Self {
            let state = Arc::new(Mutex::new(State::default()));
            let shared = state.clone();
            let mut client = Client::new("http://127.0.0.1:1", "secret").unwrap();
            client.test_handler = Some(Arc::new(move |method, path, query, body| {
                let path = format!(
                    "{path}?{}",
                    query
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("&")
                );
                let mut state = shared.lock().unwrap();
                let body = body.unwrap_or(Value::Null);
                state
                    .requests
                    .push((method.into(), path.clone(), body.clone()));
                let (status, value) = respond(&mut state, id, method, &path, body);
                if status >= 400 {
                    Err(crate::error::AppError::new("syncthing.http_error").with("status", status))
                } else {
                    Ok(value)
                }
            }));
            Self { client, state }
        }
        fn local(&self) -> LocalSync {
            LocalSync {
                address_updates: Default::default(),
                instance: None,
                client: self.client.clone(),
                id: A.into(),
            }
        }
    }
    fn respond(s: &mut State, id: &str, method: &str, path: &str, body: Value) -> (u16, Value) {
        if s.fail
            .as_deref()
            .is_some_and(|f| path.contains(f) || format!("{method} {path}").contains(f))
        {
            s.fail = None;
            return (500, json!({}));
        }
        let route = path.split('?').next().unwrap();
        if matches!(method, "PUT" | "PATCH") && route.starts_with("/rest/config/folders/") {
            s.writes += 1;
            if s.fail_writes_at.contains(&s.writes) {
                return (500, json!({}));
            }
        }
        let value = match route {
            "/rest/system/status" => json!({"myID":id}),
            "/rest/config/devices" => json!(s.devices),
            "/rest/config/folders" => json!(s.folders),
            "/rest/db/ignores" => {
                if method == "GET"
                    && let Some(response) = &s.ignores_response
                {
                    return (200, response.clone());
                }
                if method == "POST" {
                    s.rules = serde_json::from_value(body["ignore"].clone()).unwrap();
                    if s.files {
                        let fid = path
                            .split("folder=")
                            .nth(1)
                            .unwrap()
                            .split('&')
                            .next()
                            .unwrap();
                        let folder = s.folders.iter().find(|f| f["id"] == fid).unwrap();
                        let dir = Path::new(folder["path"].as_str().unwrap());
                        if dir.is_dir() {
                            fs::write(dir.join(".stignore"), format!("{}\n", s.rules.join("\n")))
                                .unwrap();
                        }
                    }
                }
                json!({"ignore":if s.rules.is_empty() { Value::Null } else { json!(s.rules) }})
            }
            "/rest/db/status" => {
                s.index_reads += 1;
                if s.folders.is_empty() && s.index_reads > 1 {
                    return (404, json!({}));
                }
                json!({"localFiles":3,"globalFiles":5,"globalBytes":50,"needBytes":10,"state":"idle","receiveOnlyTotalItems":s.receive_only_items})
            }
            "/rest/system/connections" => json!({"connections":{B:{"connected":true}}}),
            "/rest/db/completion" => {
                if path.contains(B) {
                    json!({"completion":80.0})
                } else {
                    return (503, json!({}));
                }
            }
            "/rest/folder/errors" => {
                json!({"errors":(0..105).map(|_| json!({"path":"a", "error":"权限不足"})).collect::<Vec<_>>()})
            }
            "/rest/db/scan" | "/rest/db/revert" => Value::Null,
            _ if route.starts_with("/rest/config/devices/") => {
                let device = route.rsplit('/').next().unwrap();
                if method == "PATCH" {
                    let old = s
                        .devices
                        .iter_mut()
                        .find(|d| d["deviceID"] == device)
                        .unwrap();
                    for (k, v) in body.as_object().unwrap() {
                        old[k] = v.clone();
                    }
                } else {
                    s.devices.retain(|d| d["deviceID"] != body["deviceID"]);
                    s.devices.push(body);
                }
                Value::Null
            }
            _ if route.starts_with("/rest/config/folders/") => {
                let fid = route.rsplit('/').next().unwrap();
                let index = s.folders.iter().position(|f| f["id"] == fid);
                match method {
                    "GET" => {
                        return index
                            .map(|i| (200, s.folders[i].clone()))
                            .unwrap_or((404, Value::Null));
                    }
                    "DELETE" => {
                        if let Some(i) = index {
                            let removed = s.folders.remove(i);
                            let dir = Path::new(removed["path"].as_str().unwrap());
                            if dir.join(".stfolder").exists() {
                                if fs::read_dir(dir.join(".stfolder"))
                                    .unwrap()
                                    .next()
                                    .is_none()
                                {
                                    fs::remove_dir(dir.join(".stfolder")).unwrap();
                                } else {
                                    fs::rename(
                                        dir.join(".stfolder"),
                                        dir.join(".stfolder.removed-20261022-203904"),
                                    )
                                    .unwrap();
                                }
                            }
                        }
                    }
                    "PUT" => {
                        if s.files {
                            let dir = Path::new(body["path"].as_str().unwrap());
                            if dir.is_dir() && !dir.join(".stfolder").exists() {
                                fs::create_dir(dir.join(".stfolder")).unwrap();
                            }
                        }
                        if let Some(i) = index {
                            s.folders[i] = body;
                        } else {
                            s.folders.push(body);
                        }
                    }
                    "PATCH" => {
                        let Some(i) = index else {
                            return (404, Value::Null);
                        };
                        for (k, v) in body.as_object().unwrap() {
                            s.folders[i][k] = v.clone();
                        }
                    }
                    _ => panic!("未知方法"),
                }
                if method == "PUT" || method == "PATCH" {
                    let folder = s.folders.iter_mut().find(|f| f["id"] == fid).unwrap();
                    let shared = folder["devices"].as_array_mut().unwrap();
                    if !shared.iter().any(|d| d["deviceID"] == id) {
                        shared.push(json!({"deviceID":id}));
                    }
                }
                Value::Null
            }
            _ => panic!("未知接口 {route}"),
        };
        (200, value)
    }
    fn info() -> ServerEndpointInfo {
        ServerEndpointInfo {
            server_id: "server".into(),
            name: "服务器".repeat(40),
            device_id: B.into(),
            socket_path: "/socket".into(),
            sync_port: 12345,
            api_key: "secret".into(),
            state_dir: "/state".into(),
        }
    }
    fn spec(root: &Path) -> TaskSpec {
        TaskSpec {
            data_dir: root.join("recovery-data"),
            folder_id: None,
            name: "项目".into(),
            local_path: root.join("project"),
            local_mode: FolderKind::default(),
            endpoints: vec![EndpointSpec {
                server_id: "server".into(),
                remote_path: "/project".into(),
                mode: FolderKind::ReceiveOnly,
            }],
            ignores_source: IgnoresSource::Unconfirmed,
            ignore_rules: vec![".git".into()],
            watch_delay_s: 10,
            collab: true,
            versioning: true,
            versions_root: root.join("versions"),
        }
    }
    fn editable(spec: &TaskSpec) -> TaskSettings {
        TaskSettings {
            local_mode: spec.local_mode,
            endpoints: spec
                .endpoints
                .iter()
                .map(|e| EndpointSettings {
                    server_id: e.server_id.clone(),
                    mode: e.mode,
                })
                .collect(),
            watch_delay_s: spec.watch_delay_s,
            collab: spec.collab,
            versioning: spec.versioning,
            versions_root: spec.versions_root.clone(),
        }
    }
    #[test]
    fn 本机只接收组合预检_加入和新增端点也拒绝双只接收() {
        let a = Api::new(A);
        let b = Api::new(B);
        let c = Api::new(C);
        let local = a.local();
        let root = crate::test_support::tempdir();
        let link = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info(),
            &local,
            "电脑",
        )
        .unwrap();
        let mut spec = spec(root.path());
        spec.local_mode = FolderKind::ReceiveOnly;
        let endpoints = std::mem::take(&mut spec.endpoints);
        assert_eq!(
            create_task(&local, &[&link], &spec).unwrap_err().code,
            "sync.invalid_mode"
        );
        spec.endpoints = endpoints;
        assert_eq!(
            create_task(&local, &[&link], &spec).unwrap_err().code,
            "sync.invalid_mode"
        );
        assert!(a.state.lock().unwrap().folders.is_empty());
        assert!(b.state.lock().unwrap().folders.is_empty());
        spec.endpoints[0].mode = FolderKind::SendReceive;
        let id = create_task(&local, &[&link], &spec).unwrap();
        assert_eq!(
            remove_endpoint(&local, &link, &id).unwrap_err().code,
            "sync.invalid_mode"
        );
        assert!(b.client.get_folder(&id).unwrap().is_some());
        assert_eq!(a.state.lock().unwrap().folders[0]["type"], "receiveonly");
        let mut other = info();
        other.server_id = "other".into();
        other.device_id = C.into();
        let other_link = local_link(
            c.client.clone(),
            "tcp://127.0.0.1:12346".into(),
            &other,
            &local,
            "电脑",
        )
        .unwrap();
        assert_eq!(
            add_endpoint(
                &local,
                &other_link,
                &id,
                "/other",
                FolderKind::ReceiveOnly,
                root.path()
            )
            .unwrap_err()
            .code,
            "sync.invalid_mode"
        );
        assert!(c.state.lock().unwrap().folders.is_empty());
        // 加入已有目录时也核对服务器的真实方式，不能只信本次传入的方式。
        a.client.remove_folder(&id).unwrap();
        b.state.lock().unwrap().folders[0]["type"] = json!("receiveonly");
        spec.folder_id = Some(id);
        assert_eq!(
            create_task(&local, &[&link], &spec).unwrap_err().code,
            "sync.invalid_mode"
        );
        assert!(a.state.lock().unwrap().folders.is_empty());
        spec.endpoints.push(EndpointSpec {
            server_id: "other".into(),
            remote_path: "/other".into(),
            mode: FolderKind::ReceiveOnly,
        });
        assert_eq!(
            create_task(&local, &[&link, &other_link], &spec)
                .unwrap_err()
                .code,
            "sync.invalid_mode"
        );
    }
    #[test]
    fn 编辑设置保留非设置字段_开关协作版本方向_还原入口校验() {
        let a = Api::new(A);
        let b = Api::new(B);
        let local = a.local();
        let root = crate::test_support::tempdir();
        let link = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info(),
            &local,
            "电脑",
        )
        .unwrap();
        let spec = spec(root.path());
        let id = create_task(&local, &[&link], &spec).unwrap();
        for api in [&a, &b] {
            api.state.lock().unwrap().folders[0]["custom"] = json!({"保留":"字段"});
        }
        let before: Vec<_> = [&a, &b]
            .iter()
            .map(|api| api.state.lock().unwrap().folders[0].clone())
            .collect();
        let mut settings = editable(&spec);
        assert_eq!(
            revert_local_changes(&local, &id).unwrap_err().code,
            "sync.invalid_mode"
        );
        settings.local_mode = FolderKind::ReceiveOnly;
        assert_eq!(
            update_task_settings(&local, &[&link], &id, &settings)
                .unwrap_err()
                .code,
            "sync.invalid_mode"
        );
        settings.endpoints[0].mode = FolderKind::SendReceive;
        settings.collab = false;
        settings.watch_delay_s = 17;
        settings.versioning = false;
        update_task_settings(&local, &[&link], &id, &settings).unwrap();
        for (api, old) in [&a, &b].iter().zip(&before) {
            let config = api.state.lock().unwrap().folders[0].clone();
            for key in [
                "id",
                "label",
                "path",
                "paused",
                "devices",
                "custom",
                "ignorePerms",
                "maxConflicts",
            ] {
                assert_eq!(config[key], old[key]);
            }
            assert_eq!(config["fsWatcherDelayS"], 17);
            assert_eq!(config["versioning"]["type"], "");
        }
        assert_eq!(a.state.lock().unwrap().folders[0]["type"], "receiveonly");
        assert_eq!(b.state.lock().unwrap().folders[0]["type"], "sendreceive");
        a.state.lock().unwrap().receive_only_items = 7;
        assert_eq!(
            task_status(&local, &id, &[]).unwrap().local_changed_items,
            7
        );
        revert_local_changes(&local, &id).unwrap();
        assert!(
            a.state
                .lock()
                .unwrap()
                .requests
                .iter()
                .any(|(m, p, _)| m == "POST" && p == &format!("/rest/db/revert?folder={id}"))
        );
        assert!(revert_local_changes(&local, "../bad").is_err());
        settings.local_mode = FolderKind::SendReceive;
        settings.endpoints[0].mode = FolderKind::ReceiveOnly;
        settings.collab = true;
        settings.versioning = true;
        update_task_settings(&local, &[&link], &id, &settings).unwrap();
        for api in [&a, &b] {
            let state = api.state.lock().unwrap();
            assert_eq!(state.folders[0]["fsWatcherDelayS"], 1);
            assert_eq!(state.folders[0]["versioning"]["type"], "trashcan");
            assert_eq!(
                state.folders[0]["versioning"]["params"]["cleanoutDays"],
                "30"
            );
        }
        assert_eq!(b.state.lock().unwrap().folders[0]["type"], "receiveonly");
    }
    #[test]
    fn 多端编辑预检离线_失败逆序回滚_恢复失败列出端点() {
        let a = Api::new(A);
        let b = Api::new(B);
        let c = Api::new(C);
        let local = a.local();
        let root = crate::test_support::tempdir();
        let link = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info(),
            &local,
            "电脑",
        )
        .unwrap();
        let mut other = info();
        other.server_id = "other".into();
        other.device_id = C.into();
        let mut other_link = local_link(
            c.client.clone(),
            "tcp://127.0.0.1:12346".into(),
            &other,
            &local,
            "电脑",
        )
        .unwrap();
        let mut spec = spec(root.path());
        spec.endpoints.push(EndpointSpec {
            server_id: "other".into(),
            remote_path: "/other".into(),
            mode: FolderKind::SendReceive,
        });
        let id = create_task(&local, &[&link, &other_link], &spec).unwrap();
        let originals: Vec<_> = [&a, &b, &c]
            .iter()
            .map(|api| {
                let mut state = api.state.lock().unwrap();
                state.writes = 0;
                state.folders[0].clone()
            })
            .collect();
        let mut settings = editable(&spec);
        settings.collab = false;
        settings.watch_delay_s = 33;
        settings.versioning = false;
        assert_eq!(
            update_task_settings(&local, &[&link], &id, &settings)
                .unwrap_err()
                .code,
            "sync.server_offline"
        );
        let online = other_link.remote.clone();
        other_link.remote.test_handler = Some(Arc::new(|_, _, _, _| {
            Err(AppError::new("syncthing.unreachable"))
        }));
        let offline =
            update_task_settings(&local, &[&link, &other_link], &id, &settings).unwrap_err();
        assert_eq!(offline.code, "sync.server_offline");
        assert_eq!(offline.params["server_id"], "other");
        other_link.remote = online;
        c.state.lock().unwrap().fail = Some("GET /rest/config/folders/".into());
        assert!(update_task_settings(&local, &[&link, &other_link], &id, &settings).is_err());
        for api in [&a, &b, &c] {
            assert_eq!(api.state.lock().unwrap().writes, 0);
        }
        settings.watch_delay_s = 61;
        assert!(update_task_settings(&local, &[&link, &other_link], &id, &settings).is_err());
        settings.watch_delay_s = 33;
        settings.versions_root = spec.local_path.join("versions");
        settings.versioning = true;
        assert_eq!(
            update_task_settings(&local, &[&link, &other_link], &id, &settings)
                .unwrap_err()
                .code,
            "syncthing.versions_inside_folder"
        );
        settings.versions_root = spec.versions_root.clone();
        settings.versioning = false;
        for api in [&a, &b, &c] {
            assert_eq!(api.state.lock().unwrap().writes, 0);
        }
        c.state.lock().unwrap().fail_writes_at = vec![1];
        let cause =
            update_task_settings(&local, &[&link, &other_link], &id, &settings).unwrap_err();
        assert_eq!(cause.code, "syncthing.http_error");
        assert_eq!(cause.params["endpoint"], "other");
        for (api, old) in [&a, &b, &c].iter().zip(&originals) {
            let mut state = api.state.lock().unwrap();
            assert_eq!(&state.folders[0], old);
            state.writes = 0;
        }
        a.state.lock().unwrap().fail_writes_at = vec![2];
        c.state.lock().unwrap().fail_writes_at = vec![1, 2];
        let partial =
            update_task_settings(&local, &[&link, &other_link], &id, &settings).unwrap_err();
        assert_eq!(partial.code, "sync.partial_failure");
        assert!(partial.params["failures"].contains("local:"));
        assert!(partial.params["failures"].contains("other:"));
        assert_eq!(b.state.lock().unwrap().folders[0], originals[1]);
    }
    #[test]
    fn y15_身份拒绝不写配置_配对幂等() {
        let a = Api::new(A);
        let b = Api::new(B);
        let local = a.local();
        let mut info = info();
        info.device_id = A.into();
        let result = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info,
            &local,
            "电脑",
        );
        assert_eq!(result.err().unwrap().code, "sync.device_mismatch");
        assert!(a.state.lock().unwrap().requests.is_empty());
        assert_eq!(b.state.lock().unwrap().requests.len(), 1);
        info.device_id = B.into();
        for _ in 0..2 {
            let mut link = local_link(
                b.client.clone(),
                "tcp://127.0.0.1:12345".into(),
                &info,
                &local,
                "电脑",
            )
            .unwrap();
            assert_eq!(link.local_sync_port(), 12345);
            link.refresh_address(&local).unwrap();
        }
        assert_eq!(a.state.lock().unwrap().devices.len(), 1);
        assert_eq!(
            b.state.lock().unwrap().devices[0]["addresses"],
            json!(["dynamic"])
        );
        assert_eq!(
            a.state.lock().unwrap().devices[0]["name"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            64
        );
        local.stop().unwrap();
    }
    #[test]
    fn 创建回滚与迁移恢复用户标记快照() {
        for (existed, ignore_existed, empty) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
        ] {
            let root = crate::test_support::tempdir();
            let a = Api::new(A);
            let b = Api::new(B);
            let local = a.local();
            let link = local_link(
                b.client.clone(),
                "tcp://127.0.0.1:12345".into(),
                &info(),
                &local,
                "电脑",
            )
            .unwrap();
            let mut task = spec(root.path());
            task.endpoints[0].remote_path = root.path().join("remote").display().to_string();
            for dir in [&task.local_path, &root.path().join("remote")] {
                fs::create_dir(dir).unwrap();
                if ignore_existed {
                    fs::write(dir.join(".stignore"), "// 原有\r\n规则").unwrap();
                }
                if existed {
                    fs::create_dir(dir.join(".stfolder")).unwrap();
                    if !empty {
                        fs::write(dir.join(".stfolder/user-marker"), "用户内容").unwrap();
                    }
                }
            }
            let before = directory_snapshot(root.path());
            a.state.lock().unwrap().files = true;
            b.state.lock().unwrap().files = true;
            b.state.lock().unwrap().fail = Some("/rest/db/ignores".into());
            assert!(create_task(&local, &[&link], &task).is_err());
            assert_eq!(directory_snapshot(root.path()), before);
            assert!(a.state.lock().unwrap().folders.is_empty());
            assert!(b.state.lock().unwrap().folders.is_empty());
            a.state
                .lock()
                .unwrap()
                .folders
                .push(json!({"id":ID,"path":task.local_path,"devices":[],"paused":false}));
            b.state.lock().unwrap().fail = Some("/rest/db/ignores".into());
            assert!(
                add_endpoint(
                    &local,
                    &link,
                    ID,
                    &task.endpoints[0].remote_path,
                    FolderKind::SendReceive,
                    &task.data_dir,
                )
                .is_err()
            );
            assert_eq!(directory_snapshot(root.path()), before);
            relocate_local(&local, ID, &root.path().join("new")).unwrap();
            // 迁移不恢复旧目录里任务自己的标记（Syncthing 会把它改名或移走），
            // 但标记目录里用户自己的内容不能丢
            let had_user_content = before
                .keys()
                .map(|k| k.to_string_lossy())
                .any(|k| k.starts_with("project/.stfolder") && k.ends_with("user-marker"));
            let after = directory_snapshot(root.path());
            if had_user_content {
                assert!(
                    after.iter().any(|(k, v)| {
                        let k = k.to_string_lossy();
                        k.starts_with("project/.stfolder")
                            && k.ends_with("user-marker")
                            && v.is_some()
                    }),
                    "迁移后目录: {after:?}"
                );
            }
        }
    }
    #[test]
    fn y18_y19_y20_创建加入状态移除和回滚() {
        let a = Api::new(A);
        let b = Api::new(B);
        let local = a.local();
        let root = crate::test_support::tempdir();
        let link = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info(),
            &local,
            "电脑",
        )
        .unwrap();
        let mut spec = spec(root.path());
        b.state.lock().unwrap().fail = Some("/rest/db/ignores".into());
        assert!(create_task(&local, &[&link], &spec).is_err());
        assert!(a.state.lock().unwrap().folders.is_empty());
        assert!(b.state.lock().unwrap().folders.is_empty());
        let id = create_task(&local, &[&link], &spec).unwrap();
        for api in [&a, &b] {
            let s = api.state.lock().unwrap();
            assert_eq!(s.folders[0]["paused"], true);
            assert_eq!(s.folders[0]["fsWatcherDelayS"], 1);
            assert_eq!(s.rules, [".git"]);
            assert_eq!(s.folders[0]["versioning"]["params"]["cleanoutDays"], "30");
        }
        assert_eq!(b.state.lock().unwrap().folders[0]["type"], "receiveonly");
        set_paused(&local, &[&link], &id, false).unwrap();
        rescan(&local, &[&link], &id).unwrap();
        update_ignores(&local, &[&link], &id, &["target".into()]).unwrap();
        let status = task_status(
            &local,
            &id,
            &[("server".into(), B.into()), ("offline".into(), A.into())],
        )
        .unwrap();
        assert!(status.servers[0].connected);
        assert_eq!(status.servers[0].completion, Some(80.0));
        assert!(!status.servers[1].connected);
        assert!(status.servers[1].error.is_some());
        assert_eq!(status.errors.len(), 100);
        let join = find_joinable(&link, "/project/sub/..").unwrap().unwrap();
        assert_eq!(join.folder_id, id);
        assert_eq!(join.ignores, ["target"]);
        a.client.remove_folder(&id).unwrap();
        b.state.lock().unwrap().folders[0]["devices"] = json!([{"deviceID":B}]);
        let old = b.state.lock().unwrap().folders[0].clone();
        spec.folder_id = Some(id.clone());
        create_task(&local, &[&link], &spec).unwrap();
        let mut expected = old;
        expected["devices"] = json!([{"deviceID":B},{"deviceID":A}]);
        assert_eq!(b.state.lock().unwrap().folders[0], expected);
        assert_eq!(a.state.lock().unwrap().rules, ["target"]);
        remove_endpoint(&local, &link, &id).unwrap();
        assert!(b.state.lock().unwrap().folders.is_empty());
        assert_eq!(
            a.state.lock().unwrap().folders[0]["devices"],
            json!([{"deviceID":A}])
        );
        remove_task(&local, &[], &id).unwrap();
        assert!(a.state.lock().unwrap().folders.is_empty());
    }
    #[test]
    fn 多服务器点对点拓扑_加入失败恢复共享_配置预检() {
        let a = Api::new(A);
        let b = Api::new(B);
        let c = Api::new(C);
        let local = a.local();
        let b_link = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info(),
            &local,
            "电脑",
        )
        .unwrap();
        let mut c_info = info();
        c_info.server_id = "second".into();
        c_info.device_id = C.into();
        let c_link = local_link(
            c.client.clone(),
            "tcp://127.0.0.1:12346".into(),
            &c_info,
            &local,
            "电脑",
        )
        .unwrap();
        let root = crate::test_support::tempdir();
        let mut spec = spec(root.path());
        spec.collab = false;
        spec.versioning = false;
        spec.endpoints.push(EndpointSpec {
            server_id: "second".into(),
            remote_path: "/second".into(),
            mode: FolderKind::SendReceive,
        });
        let id = create_task(&local, &[&b_link, &c_link], &spec).unwrap();
        for (api, own_id) in [(&b, B), (&c, C)] {
            let state = api.state.lock().unwrap();
            assert_eq!(
                state.folders[0]["devices"],
                json!([{"deviceID":A},{"deviceID":own_id}])
            );
            assert_eq!(state.folders[0]["versioning"]["type"], "");
            assert_eq!(state.folders[0]["fsWatcherDelayS"], 10);
        }
        assert_eq!(
            a.state.lock().unwrap().folders[0]["devices"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        b.state.lock().unwrap().fail = Some("PATCH /rest/config/folders/".into());
        let e = set_paused(&local, &[&b_link, &c_link], &id, false).unwrap_err();
        assert_eq!(e.code, "sync.partial_failure");
        assert_eq!(e.params["failures"], "server:syncthing.http_error");
        assert_eq!(a.state.lock().unwrap().folders[0]["paused"], false);
        assert_eq!(b.state.lock().unwrap().folders[0]["paused"], true);
        assert_eq!(c.state.lock().unwrap().folders[0]["paused"], false);
        a.client.remove_folder(&id).unwrap();
        b.state.lock().unwrap().folders[0]["devices"] =
            json!([{"deviceID":B},{"deviceID":C,"encryptionPassword":"保留原设置"}]);
        let before = b.state.lock().unwrap().folders[0].clone();
        c.state.lock().unwrap().folders.clear();
        c.state.lock().unwrap().fail = Some("/rest/db/ignores".into());
        spec.folder_id = Some(id);
        assert!(create_task(&local, &[&b_link, &c_link], &spec).is_err());
        assert_eq!(b.state.lock().unwrap().folders[0], before);
        assert!(a.state.lock().unwrap().folders.is_empty());
        assert!(c.state.lock().unwrap().folders.is_empty());
        let count = b.state.lock().unwrap().requests.len();
        spec.watch_delay_s = 0;
        assert_eq!(
            create_task(&local, &[&b_link, &c_link], &spec)
                .unwrap_err()
                .code,
            "sync.invalid_input"
        );
        assert_eq!(b.state.lock().unwrap().requests.len(), count);
    }
    #[test]
    fn 空规则兼容null且拒绝异常响应() {
        let a = Api::new(A);
        let b = Api::new(B);
        let local = a.local();
        let root = crate::test_support::tempdir();
        let link = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info(),
            &local,
            "电脑",
        )
        .unwrap();
        let mut spec = spec(root.path());
        let id = create_task(&local, &[&link], &spec).unwrap();
        a.client.remove_folder(&id).unwrap();
        spec.folder_id = Some(id.clone());
        for response in [json!({"ignore":null}), json!({"ignore":[]})] {
            b.state.lock().unwrap().ignores_response = Some(response);
            assert!(
                find_joinable(&link, "/project")
                    .unwrap()
                    .unwrap()
                    .ignores
                    .is_empty()
            );
            let e = create_task(&local, &[&link], &spec).unwrap_err();
            assert_eq!(e.code, "sync.ignores_mismatch");
            assert_eq!(e.params["server_count"], "0");
            assert!(a.state.lock().unwrap().folders.is_empty());
        }
        for response in [
            json!({}),
            json!({"ignore":"错误类型"}),
            json!({"ignore":[1]}),
            json!({"ignore":null,"error":"读取失败"}),
        ] {
            b.state.lock().unwrap().ignores_response = Some(response);
            assert_eq!(
                find_joinable(&link, "/project").unwrap_err().code,
                "sync.invalid_response"
            );
            assert!(a.state.lock().unwrap().folders.is_empty());
        }
    }
    #[test]
    fn 唯一电脑暂停恢复和移除排除服务器自身() {
        for remove_whole_task in [false, true] {
            let a = Api::new(A);
            let b = Api::new(B);
            let local = a.local();
            let root = crate::test_support::tempdir();
            let link = local_link(
                b.client.clone(),
                "tcp://127.0.0.1:12345".into(),
                &info(),
                &local,
                "电脑",
            )
            .unwrap();
            let id = create_task(&local, &[&link], &spec(root.path())).unwrap();
            assert_eq!(
                b.state.lock().unwrap().folders[0]["devices"],
                json!([{"deviceID":A},{"deviceID":B}])
            );
            for paused in [false, true, false] {
                set_paused(&local, &[&link], &id, paused).unwrap();
                for api in [&a, &b] {
                    assert_eq!(api.state.lock().unwrap().folders[0]["paused"], paused);
                }
            }
            if remove_whole_task {
                remove_task(&local, &[&link], &id).unwrap();
                assert!(a.state.lock().unwrap().folders.is_empty());
            } else {
                remove_endpoint(&local, &link, &id).unwrap();
                assert_eq!(
                    a.state.lock().unwrap().folders[0]["devices"],
                    json!([{"deviceID":A}])
                );
            }
            assert!(b.state.lock().unwrap().folders.is_empty());
        }
    }
    #[test]
    fn 共享服务器暂停和移除只影响本机() {
        let a = Api::new(A);
        let b = Api::new(B);
        let local = a.local();
        let root = crate::test_support::tempdir();
        let link = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info(),
            &local,
            "电脑",
        )
        .unwrap();
        for remove_whole_task in [false, true] {
            let id = create_task(&local, &[&link], &spec(root.path())).unwrap();
            let other = json!({"deviceID":C,"encryptionPassword":"原配置"});
            b.state.lock().unwrap().folders[0]["devices"]
                .as_array_mut()
                .unwrap()
                .push(other.clone());
            set_paused(&local, &[&link], &id, false).unwrap();
            assert_eq!(a.state.lock().unwrap().folders[0]["paused"], false);
            assert_eq!(b.state.lock().unwrap().folders[0]["paused"], true);
            if remove_whole_task {
                remove_task(&local, &[&link], &id).unwrap();
            } else {
                remove_endpoint(&local, &link, &id).unwrap();
                remove_task(&local, &[], &id).unwrap();
            }
            assert_eq!(
                b.state.lock().unwrap().folders[0]["devices"],
                json!([{"deviceID":B},other])
            );
            assert!(a.state.lock().unwrap().folders.is_empty());
            b.client.remove_folder(&id).unwrap();
        }
    }
    #[test]
    fn 新增端点成功和失败逆序回滚() {
        let a = Api::new(A);
        let b = Api::new(B);
        let local = a.local();
        let root = crate::test_support::tempdir();
        let link = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info(),
            &local,
            "电脑",
        )
        .unwrap();
        let config = crate::syncthing::folder::FolderSpec {
            id: ID.into(),
            devices: vec![A.into()],
            path: root.path().join("files"),
            versions_dir: Some(root.path().join("versions")),
            ..Default::default()
        }
        .to_config()
        .unwrap();
        a.state.lock().unwrap().folders.push(config.clone());
        a.state.lock().unwrap().rules = vec!["target".into()];
        b.state.lock().unwrap().fail = Some("/rest/db/ignores".into());
        assert!(
            add_endpoint(
                &local,
                &link,
                ID,
                "/project",
                FolderKind::SendReceive,
                std::path::Path::new("/unused")
            )
            .is_err()
        );
        assert_eq!(a.state.lock().unwrap().folders[0], config);
        assert!(b.state.lock().unwrap().folders.is_empty());
        add_endpoint(
            &local,
            &link,
            ID,
            "/project",
            FolderKind::ReceiveOnly,
            std::path::Path::new("/unused"),
        )
        .unwrap();
        assert_eq!(b.state.lock().unwrap().rules, ["target"]);
        assert_eq!(b.state.lock().unwrap().folders[0]["paused"], true);
        assert_eq!(b.state.lock().unwrap().folders[0]["type"], "receiveonly");
        assert_eq!(
            b.state.lock().unwrap().folders[0]["versioning"]["fsPath"],
            format!("/state/versions/{ID}")
        );
        assert_eq!(
            a.state.lock().unwrap().folders[0]["devices"],
            json!([{"deviceID":A},{"deviceID":B}])
        );
        assert!(
            add_endpoint(
                &local,
                &link,
                ID,
                "/project",
                FolderKind::SendReceive,
                std::path::Path::new("/unused")
            )
            .is_err()
        );
        assert!(!a.state.lock().unwrap().requests.iter().any(|(m, p, _)| {
            p.starts_with("/rest/config/folders/") && (m == "DELETE" || m == "PUT")
        }));
        assert_eq!(
            b.state.lock().unwrap().folders[0]["devices"],
            json!([{"deviceID":A},{"deviceID":B}])
        );
        set_paused(&local, &[&link], ID, false).unwrap();
        assert_eq!(b.state.lock().unwrap().folders[0]["paused"], false);
        remove_endpoint(&local, &link, ID).unwrap();
        assert!(b.state.lock().unwrap().folders.is_empty());
        assert_eq!(
            a.state.lock().unwrap().folders[0]["devices"],
            json!([{"deviceID":A}])
        );
    }
    #[test]
    fn 规则确认_覆盖失败恢复_外来文件夹() {
        let a = Api::new(A);
        let b = Api::new(B);
        let local = a.local();
        let root = crate::test_support::tempdir();
        let link = local_link(
            b.client.clone(),
            "tcp://127.0.0.1:12345".into(),
            &info(),
            &local,
            "电脑",
        )
        .unwrap();
        let mut spec = spec(root.path());
        spec.folder_id = Some(ID.into());
        let old = json!({"id":ID,"path":"/project","devices":[{"deviceID":B},{"deviceID":C}],"paused":false});
        b.state.lock().unwrap().folders.push(old.clone());
        let e = create_task(&local, &[&link], &spec).unwrap_err();
        assert_eq!(e.code, "sync.ignores_mismatch");
        assert_eq!(e.params["server_count"], "0");
        assert_eq!(e.params["local_count"], "1");
        assert!(a.state.lock().unwrap().folders.is_empty());
        spec.ignores_source = IgnoresSource::Server;
        create_task(&local, &[&link], &spec).unwrap();
        assert!(a.state.lock().unwrap().rules.is_empty());
        remove_task(&local, &[&link], ID).unwrap();
        spec.ignores_source = IgnoresSource::Local;
        b.state.lock().unwrap().fail = Some("/rest/db/ignores".into());
        // 第一次读取规则也会失败，不允许预检失败后写入。
        assert!(create_task(&local, &[&link], &spec).is_err());
        assert_eq!(b.state.lock().unwrap().folders[0], old);
        b.state.lock().unwrap().fail = Some("POST /rest/db/ignores".into());
        assert!(create_task(&local, &[&link], &spec).is_err());
        assert_eq!(b.state.lock().unwrap().folders[0], old);
        assert!(b.state.lock().unwrap().rules.is_empty());
        assert!(a.state.lock().unwrap().folders.is_empty());
        create_task(&local, &[&link], &spec).unwrap();
        assert_eq!(b.state.lock().unwrap().rules, [".git"]);
        assert_eq!(link.folder_paths().unwrap(), ["/project"]);
        b.state.lock().unwrap().folders[0]["id"] = json!("foreign");
        let e = find_joinable(&link, "/project").unwrap_err();
        assert_eq!(e.code, "sync.foreign_folder");
        assert_eq!(e.params["path"], "/project");
        assert_eq!(e.params["folder_id"], "foreign");
    }
    #[test]
    fn 迁移各阶段错误可见() {
        for (failure, stage, rebuilt) in [
            ("/rest/db/status", "index_clear", false),
            ("PUT /rest/config/folders/", "readd", false),
            ("POST /rest/db/ignores", "ignores", true),
        ] {
            let api = Api::new(A);
            let root = crate::test_support::tempdir();
            api.state
                .lock()
                .unwrap()
                .folders
                .push(json!({"id":ID,"path":root.path().join("old"),"devices":[],"paused":false}));
            api.state.lock().unwrap().fail = Some(failure.into());
            let e = relocate_local(&api.local(), ID, &root.path().join("New")).unwrap_err();
            assert_eq!(e.code, "sync.relocate_incomplete");
            assert_eq!(e.params["stage"], stage);
            assert_eq!(e.params["folder_id"], ID);
            let state = api.state.lock().unwrap();
            assert_eq!(!state.folders.is_empty(), rebuilt);
            if rebuilt {
                assert_eq!(state.folders[0]["paused"], true);
            }
        }
    }
    #[test]
    fn 配对保留限速且仅修改变化字段() {
        let a = Api::new(A);
        let b = Api::new(B);
        let local = a.local();
        let info = info();
        for api in [&a, &b] {
            api.state.lock().unwrap().devices.push(json!({"deviceID":if std::ptr::eq(api, &a) {B} else {A}, "name":"旧名", "addresses":["dynamic"], "maxSendKbps":123, "maxRecvKbps":456}));
        }
        for _ in 0..2 {
            drop(
                local_link(
                    b.client.clone(),
                    "tcp://127.0.0.1:12345".into(),
                    &info,
                    &local,
                    "电脑",
                )
                .unwrap(),
            );
        }
        for api in [&a, &b] {
            let state = api.state.lock().unwrap();
            assert_eq!(state.devices[0]["maxSendKbps"], 123);
            assert_eq!(state.devices[0]["maxRecvKbps"], 456);
            let patches: Vec<_> = state
                .requests
                .iter()
                .filter(|(m, _, _)| m == "PATCH")
                .collect();
            assert_eq!(patches.len(), 1);
            assert!(!state.requests.iter().any(|(m, _, _)| m == "PUT"));
            assert!(patches[0].2.get("maxSendKbps").is_none());
        }
    }
    #[test]
    fn y3_索引清理超时禁止重建() {
        let api = Api::new(A);
        let root = crate::test_support::tempdir();
        api.state
            .lock()
            .unwrap()
            .folders
            .push(json!({"id":ID,"path":root.path().join("old"),"devices":[],"paused":false}));
        let error = super::super::tasks::relocate(
            &api.client,
            ID,
            &root.path().join("new"),
            Duration::ZERO,
        )
        .unwrap_err();
        assert_eq!(error.code, "sync.relocate_incomplete");
        assert_eq!(error.params["stage"], "index_clear");
        assert!(
            !api.state
                .lock()
                .unwrap()
                .requests
                .iter()
                .any(|(method, _, _)| method == "PUT")
        );
    }
    #[test]
    fn y3_迁移严格等待索引删除后才重建() {
        let api = Api::new(A);
        let local = api.local();
        let root = crate::test_support::tempdir();
        api.state
            .lock()
            .unwrap()
            .folders
            .push(json!({"id":ID,"path":root.path().join("old"),"devices":[],"paused":false}));
        relocate_local(&local, ID, &root.path().join("new")).unwrap();
        let state = api.state.lock().unwrap();
        let sequence: Vec<_> = state
            .requests
            .iter()
            .filter(|(m, p, _)| m == "DELETE" || m == "PUT" || p.starts_with("/rest/db/status"))
            .map(|(m, p, _)| (m.as_str(), p.split('?').next().unwrap()))
            .collect();
        assert_eq!(
            sequence,
            [
                ("DELETE", "/rest/config/folders/pl-1234567890123456"),
                ("GET", "/rest/db/status"),
                ("GET", "/rest/db/status"),
                ("PUT", "/rest/config/folders/pl-1234567890123456")
            ]
        );
        assert_eq!(state.folders[0]["paused"], true);
    }
}

#[cfg(unix)]
#[test]
fn y8_y24_d13_y22_远程真实路径脚本() {
    use crate::{process::SshProgram, ssh::args::SshCommand};
    let root = crate::test_support::tempdir();
    let home = root.path().join("home");
    fs::create_dir_all(home.join(".ssh")).unwrap();
    let program = SshProgram {
        path: "/bin/sh".into(),
    };
    let cmd = SshCommand {
        args: vec!["-s".into()],
        env: vec![("HOME".into(), home.display().to_string())],
        config_text: None,
        hooked: false,
        builtin: None,
        askpass: None,
    };
    for path in [
        "/",
        "~",
        "/etc/../usr/bin",
        "/run/user",
        "/lib32/x",
        "/var",
        "~/.ssh",
        "~/.local/state/pierlink/x",
    ] {
        let (_, issues) = check_remote_path(&program, &cmd, path, &[], &[]).unwrap();
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, PathIssue::Dangerous { .. })),
            "{path}"
        );
    }
    assert!(
        !check_remote_path(&program, &cmd, "/var/projects", &[], &[])
            .unwrap()
            .1
            .iter()
            .any(|i| matches!(i, PathIssue::Dangerous { .. }))
    );
    let work = home.join("work '$(false)");
    fs::create_dir(&work).unwrap();
    fs::create_dir(work.join(".stfolder")).unwrap();
    fs::write(work.join(".stignore"), "").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&work)
            .status()
            .unwrap()
            .success()
    );
    let (info, issues) =
        check_remote_path(&program, &cmd, work.to_str().unwrap(), &[], &[]).unwrap();
    assert!(info.git_repository);
    assert!(info.has_ignore_file);
    assert!(
        issues
            .iter()
            .any(|i| matches!(i, PathIssue::OtherSyncthing { .. }))
    );
    let (_, managed) = check_remote_path(
        &program,
        &cmd,
        work.to_str().unwrap(),
        &[],
        std::slice::from_ref(&info.canonical_path),
    )
    .unwrap();
    assert!(
        !managed
            .iter()
            .any(|i| matches!(i, PathIssue::OtherSyncthing { .. }))
    );
    fs::create_dir(work.join("child")).unwrap();
    let (child, _) = check_remote_path(
        &program,
        &cmd,
        work.join("child").to_str().unwrap(),
        &[],
        &[],
    )
    .unwrap();
    assert!(!child.git_repository);
    fs::write(work.join("child/.git"), "gitdir: ../.git").unwrap();
    let (child, _) = check_remote_path(
        &program,
        &cmd,
        work.join("child").to_str().unwrap(),
        &[],
        &[],
    )
    .unwrap();
    assert!(child.git_repository);
    std::os::unix::fs::symlink(&work, home.join("alias")).unwrap();
    let existing = [ExistingEndpoint {
        task_id: "task".into(),
        folder_id: ID.into(),
        remote_path: work.display().to_string(),
    }];
    let (_, issues) = check_remote_path(&program, &cmd, "~/alias", &existing, &[]).unwrap();
    assert!(issues.contains(&PathIssue::JoinExisting {
        folder_id: ID.into()
    }));
    assert!(
        !issues
            .iter()
            .any(|i| matches!(i, PathIssue::OtherSyncthing { .. }))
    );
    let (_, issues) = check_remote_path(
        &program,
        &cmd,
        &format!("{}/child", work.display()),
        &existing,
        &[],
    )
    .unwrap();
    assert!(
        issues
            .iter()
            .any(|i| matches!(i, PathIssue::Overlap { .. }))
    );
}

fn real_bin() -> Option<SyncthingBin> {
    let bin = std::env::var_os("PIERLINK_TEST_SYNCTHING");
    if bin.is_none() {
        eprintln!("跳过真实进程测试：须设置 PIERLINK_TEST_SYNCTHING 并允许监听端口");
    }
    bin.map(|path| SyncthingBin { path: path.into() })
}
fn real_local(bin: &SyncthingBin, home: &Path) -> (LocalSync, u16) {
    let gui = crate::tunnel::allocate_port().unwrap();
    let mut sync = crate::tunnel::allocate_port().unwrap();
    while sync == gui {
        sync = crate::tunnel::allocate_port().unwrap();
    }
    (
        LocalSync::start(bin, home, gui, sync, "test-key", "test-password").unwrap(),
        sync,
    )
}
fn wait(mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(60);
    while !condition() {
        assert!(Instant::now() < until, "等待同步超时");
        thread::sleep(Duration::from_millis(100));
    }
}
// 用本机实例模拟服务器，服务器的状态目录必须是 Linux 路径，Windows 上无法模拟
#[cfg(unix)]
#[test]
fn 真实进程_双向同步_只接收_迁移_三端共享_移除保留文件() {
    let Some(bin) = real_bin() else {
        return;
    };
    let root = crate::test_support::tempdir();
    let (a, _) = real_local(&bin, &root.path().join("a-home"));
    let (b, port) = real_local(&bin, &root.path().join("b-home"));
    let mut info = ServerEndpointInfo {
        server_id: "b".into(),
        name: "服务器".into(),
        device_id: b.device_id().into(),
        socket_path: "/unused".into(),
        sync_port: port,
        api_key: "test-key".into(),
        state_dir: root.path().join("b-state").display().to_string(),
    };
    info.device_id = A.into();
    assert_eq!(
        local_link(
            b.client().clone(),
            format!("tcp://127.0.0.1:{port}"),
            &info,
            &a,
            "电脑"
        )
        .err()
        .unwrap()
        .code,
        "sync.device_mismatch"
    );
    info.device_id = b.device_id().into();
    let mut link = local_link(
        b.client().clone(),
        format!("tcp://127.0.0.1:{port}"),
        &info,
        &a,
        "电脑",
    )
    .unwrap();
    drop(
        local_link(
            b.client().clone(),
            format!("tcp://127.0.0.1:{port}"),
            &info,
            &a,
            "电脑",
        )
        .unwrap(),
    );
    link.refresh_address(&a).unwrap();
    assert_eq!(
        a.client()
            .list_devices()
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["deviceID"] == b.device_id())
            .count(),
        1
    );
    let ap = root.path().join("a-files");
    let bp = root.path().join("b-files");
    fs::create_dir(&ap).unwrap();
    fs::create_dir(&bp).unwrap();
    fs::write(ap.join("a.txt"), "来自电脑").unwrap();
    fs::write(bp.join("b.txt"), "来自服务器").unwrap();
    let mut spec = TaskSpec {
        data_dir: root.path().join("recovery-data"),
        folder_id: None,
        name: "项目".into(),
        local_path: ap.clone(),
        local_mode: FolderKind::default(),
        endpoints: vec![EndpointSpec {
            server_id: "b".into(),
            remote_path: bp.display().to_string(),
            mode: FolderKind::SendReceive,
        }],
        ignores_source: IgnoresSource::Unconfirmed,
        ignore_rules: vec![".git".into()],
        watch_delay_s: 1,
        collab: true,
        versioning: true,
        versions_root: root.path().join("versions"),
    };
    let id = create_task(&a, &[&link], &spec).unwrap();
    for client in [a.client(), b.client()] {
        assert_eq!(client.get_folder(&id).unwrap().unwrap()["paused"], true);
    }
    set_paused(&a, &[&link], &id, false).unwrap();
    wait(|| ap.join("b.txt").exists() && bp.join("a.txt").exists());
    rescan(&a, &[&link], &id).unwrap();
    let before = b.client().db_status(&id).unwrap()["localFiles"]
        .as_u64()
        .unwrap();
    assert_eq!(before, 2);
    let relocated = root.path().join("a-new");
    fs::create_dir(&relocated).unwrap();
    relocate_local(&a, &id, &relocated).unwrap();
    assert_eq!(a.client().get_folder(&id).unwrap().unwrap()["paused"], true);
    set_paused(&a, &[&link], &id, false).unwrap();
    wait(|| relocated.join("b.txt").exists() && relocated.join("a.txt").exists());
    assert_eq!(b.client().db_status(&id).unwrap()["localFiles"], before);
    let (c, _) = real_local(&bin, &root.path().join("c-home"));
    let c_link = local_link(
        b.client().clone(),
        format!("tcp://127.0.0.1:{port}"),
        &info,
        &c,
        "另一电脑",
    )
    .unwrap();
    let join = find_joinable(&c_link, bp.to_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(join.folder_id, id);
    #[cfg(unix)]
    {
        let program = crate::process::SshProgram {
            path: "/bin/sh".into(),
        };
        let cmd = crate::ssh::args::SshCommand {
            args: vec!["-s".into()],
            env: vec![],
            config_text: None,
            hooked: false,
            builtin: None,
            askpass: None,
        };
        let (_, issues) = check_remote_path(
            &program,
            &cmd,
            bp.to_str().unwrap(),
            &[],
            &c_link.folder_paths().unwrap(),
        )
        .unwrap();
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, PathIssue::OtherSyncthing { .. }))
        );
    }
    spec.folder_id = Some(id.clone());
    spec.local_path = root.path().join("c-files");
    fs::create_dir(&spec.local_path).unwrap();
    spec.ignore_rules = vec!["不应覆盖服务器".into()];
    b.client().set_ignores(&id, &[]).unwrap();
    let mismatch = create_task(&c, &[&c_link], &spec).unwrap_err();
    assert_eq!(mismatch.code, "sync.ignores_mismatch");
    assert_eq!(mismatch.params["server_count"], "0");
    assert!(c.client().get_folder(&id).unwrap().is_none());
    spec.ignores_source = IgnoresSource::Server;
    create_task(&c, &[&c_link], &spec).unwrap();
    set_paused(&c, &[], &id, false).unwrap();
    wait(|| spec.local_path.join("a.txt").exists() && spec.local_path.join("b.txt").exists());
    fs::write(spec.local_path.join("c.txt"), "第三份").unwrap();
    rescan(&c, &[], &id).unwrap();
    wait(|| relocated.join("c.txt").exists() && bp.join("c.txt").exists());
    update_ignores(&a, &[&link], &id, &[".git".into(), "target".into()]).unwrap();
    assert_eq!(
        task_status(&a, &id, &[("b".into(), b.device_id().into())])
            .unwrap()
            .servers
            .len(),
        1
    );
    set_paused(&c, &[&c_link], &id, true).unwrap();
    assert_eq!(c.client().get_folder(&id).unwrap().unwrap()["paused"], true);
    assert_eq!(
        b.client().get_folder(&id).unwrap().unwrap()["paused"],
        false
    );
    assert_eq!(
        a.client().get_folder(&id).unwrap().unwrap()["paused"],
        false
    );
    set_paused(&c, &[&c_link], &id, false).unwrap();
    remove_endpoint(&c, &c_link, &id).unwrap();
    let remaining = b.client().get_folder(&id).unwrap().unwrap();
    assert!(
        remaining["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["deviceID"] == a.device_id())
    );
    assert!(
        !remaining["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["deviceID"] == c.device_id())
    );
    remove_task(&c, &[], &id).unwrap();
    spec.ignores_source = IgnoresSource::Server;
    create_task(&c, &[&c_link], &spec).unwrap();
    remove_task(&c, &[&c_link], &id).unwrap();
    fs::write(relocated.join("after-remove"), "其他电脑仍在同步").unwrap();
    rescan(&a, &[&link], &id).unwrap();
    wait(|| bp.join("after-remove").exists());
    remove_task(&a, &[&link], &id).unwrap();
    assert!(b.client().get_folder(&id).unwrap().is_none());
    for path in [&relocated, &bp, &spec.local_path] {
        assert!(path.join("c.txt").is_file());
    }
    // 独立目录验证只接收端的新增文件不会回传。
    spec.folder_id = None;
    spec.local_path = root.path().join("receive-a");
    spec.endpoints[0].remote_path = root.path().join("receive-b").display().to_string();
    spec.endpoints[0].mode = FolderKind::ReceiveOnly;
    spec.versioning = false;
    fs::create_dir(&spec.local_path).unwrap();
    fs::create_dir(&spec.endpoints[0].remote_path).unwrap();
    fs::write(spec.local_path.join("seed"), "seed").unwrap();
    let receive_id = create_task(&a, &[&link], &spec).unwrap();
    set_paused(&a, &[&link], &receive_id, false).unwrap();
    let receiver = Path::new(&spec.endpoints[0].remote_path);
    wait(|| receiver.join("seed").exists());
    fs::write(receiver.join("private"), "只在接收端").unwrap();
    rescan(&a, &[&link], &receive_id).unwrap();
    wait(|| {
        b.client()
            .db_status(&receive_id)
            .is_ok_and(|v| v["receiveOnlyChangedFiles"].as_u64().unwrap_or(0) > 0)
    });
    thread::sleep(Duration::from_secs(2));
    assert!(!spec.local_path.join("private").exists());
    remove_endpoint(&a, &link, &receive_id).unwrap();
    assert!(receiver.join("private").exists());
    remove_task(&a, &[], &receive_id).unwrap();
    drop(c_link);
    drop(link);
    c.stop().unwrap();
    a.stop().unwrap();
    b.stop().unwrap();
}

#[cfg(windows)]
#[test]
fn windows_规范路径保留大小写且去扩展前缀() {
    let root = crate::test_support::tempdir();
    let path = root.path().join("MixedCase");
    fs::create_dir(&path).unwrap();
    let canonical = fs::canonicalize(&path).unwrap();
    assert!(canonical.to_str().unwrap().starts_with(r"\\?\"));
    let normalized = super::paths::local_normalized(&canonical).unwrap();
    assert!(!normalized.to_str().unwrap().starts_with(r"\\?\"));
    assert!(normalized.to_str().unwrap().ends_with("MixedCase"));
}

// 用本机实例模拟服务器，服务器的状态目录必须是 Linux 路径，Windows 上无法模拟
#[cfg(unix)]
#[test]
fn 真实进程_预估取消删除事件与旧版本恢复() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let Some(bin) = real_bin() else {
        return;
    };
    let root = crate::test_support::tempdir();
    let (a, _) = real_local(&bin, &root.path().join("a-home"));
    let (b, port) = real_local(&bin, &root.path().join("b-home"));
    let info = ServerEndpointInfo {
        server_id: "b".into(),
        name: "服务器".into(),
        device_id: b.device_id().into(),
        socket_path: "/unused".into(),
        sync_port: port,
        api_key: "test-key".into(),
        state_dir: root.path().join("b-state").display().to_string(),
    };
    let mut link = local_link(
        b.client().clone(),
        format!("tcp://127.0.0.1:{port}"),
        &info,
        &a,
        "电脑",
    )
    .unwrap();
    link.set_estimate_file_access(EstimateFileAccess::Local);
    let left = root.path().join("left");
    let right = root.path().join("right");
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();
    fs::write(left.join("upload"), "up").unwrap();
    fs::write(right.join("download"), "down").unwrap();
    for (dir, text) in [(&left, "one"), (&right, "different")] {
        fs::write(dir.join("conflict"), text).unwrap();
        fs::write(dir.join("same"), "same").unwrap();
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::write(dir.join("node_modules/ignored"), "ignored").unwrap();
    }
    let spec = EstimateSpec {
        data_dir: root.path().join("data"),
        local_path: left.clone(),
        remote_path: right.display().to_string(),
        ignore_rules: vec!["(?d)node_modules".into()],
    };
    let before_left = directory_snapshot(&left);
    let before_right = directory_snapshot(&right);
    let estimate_result = estimate(&a, &link, &spec, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        (
            estimate_result.upload.files,
            estimate_result.download.files,
            estimate_result.same.files,
            estimate_result.conflict.files
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(estimate_result.local_total.files, 4);
    assert_eq!(estimate_result.local_sync.files, 3);
    for client in [a.client(), b.client()] {
        assert!(
            client
                .list_folders()
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .all(|f| !f["id"].as_str().unwrap().starts_with("pl-est-"))
        );
    }
    assert!(!left.join(".stfolder").exists());
    assert!(!right.join(".stfolder").exists());
    assert_eq!(directory_snapshot(&left), before_left);
    assert_eq!(directory_snapshot(&right), before_right);
    // 覆盖已有规则的 CRLF、空行、无末尾换行及已有非空标记目录。
    fs::create_dir(left.join(".stfolder")).unwrap();
    fs::write(left.join(".stfolder/user-marker"), "保留标记").unwrap();
    fs::write(left.join(".stignore"), b"// left\r\n\r\noriginal").unwrap();
    fs::write(right.join(".stignore"), b"// right\r\n\r\noriginal").unwrap();
    let before_left = directory_snapshot(&left);
    let before_right = directory_snapshot(&right);
    estimate(&a, &link, &spec, &|_| {}, &AtomicBool::new(false)).unwrap();
    assert_eq!(directory_snapshot(&left), before_left);
    assert_eq!(directory_snapshot(&right), before_right);
    let cancel = AtomicBool::new(false);
    let cancelled_at = std::cell::Cell::new(None);
    assert_eq!(
        estimate(
            &a,
            &link,
            &spec,
            &|p| {
                if p.scanned_local > 0 && !cancel.swap(true, Ordering::Relaxed) {
                    cancelled_at.set(Some(Instant::now()));
                }
            },
            &cancel
        )
        .unwrap_err()
        .code,
        "sync.cancelled"
    );
    assert!(cancelled_at.get().unwrap().elapsed() < Duration::from_secs(1));
    assert_eq!(directory_snapshot(&left), before_left);
    assert_eq!(directory_snapshot(&right), before_right);
    // 在真实扫描目录中制造冲突文件，分别核验两种选择和存档内容。
    for keep in [Keep::Current, Keep::Other] {
        fs::write(left.join("choice.txt"), "当前版本").unwrap();
        fs::write(
            left.join("choice.sync-conflict-20261022-123456-ABCDEFG.txt"),
            "另一版本",
        )
        .unwrap();
        let conflict = list_conflicts(&left, &[], 20).unwrap().conflicts.remove(0);
        resolve_conflict(&left, &root.path().join("archive"), &conflict, keep).unwrap();
        assert_eq!(
            fs::read_to_string(left.join("choice.txt")).unwrap(),
            if keep == Keep::Current {
                "当前版本"
            } else {
                "另一版本"
            }
        );
    }
    // 同一真实实例上验证本机删除事件和回收站旧版本恢复。
    let folder = crate::syncthing::folder::FolderSpec {
        id: ID.into(),
        path: left.clone(),
        paused: false,
        versions_dir: Some(root.path().join("versions")),
        ..Default::default()
    };
    a.client().put_folder(&folder).unwrap();
    for n in 0..150 {
        fs::write(left.join(format!("delete-{n}")), "x").unwrap();
    }
    a.client().scan(ID, "").unwrap();
    let mut pump = EventPump::new(&a);
    let _ = pump.poll().unwrap();
    for n in 0..150 {
        fs::remove_file(left.join(format!("delete-{n}"))).unwrap();
    }
    a.client().scan(ID, "").unwrap();
    let mut guard = DeletionGuard::new(100, 0.2, Duration::from_secs(60));
    let mut triggered = false;
    wait(|| {
        let events = pump.poll().unwrap();
        if !guard
            .observe(
                &events,
                &std::collections::HashMap::from([(ID.into(), 154)]),
            )
            .is_empty()
        {
            triggered = true;
        }
        triggered
    });
    // 构造回收站版本文件，接口按真实版本器读取并恢复。
    fs::create_dir_all(root.path().join("versions")).unwrap();
    fs::write(root.path().join("versions/restore.txt"), "旧内容").unwrap();
    fs::write(left.join("restore.txt"), "当前内容").unwrap();
    let versions = list_versions(&a, ID, Some("restore.txt")).unwrap();
    assert_eq!(versions.len(), 1);
    assert!(
        restore_versions(
            &a,
            ID,
            &[("restore.txt".into(), versions[0].version_time_ms)]
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        fs::read_to_string(left.join("restore.txt")).unwrap(),
        "旧内容"
    );
    a.stop().unwrap();
    b.stop().unwrap();
}

pub(super) fn directory_snapshot(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Option<Vec<u8>>> {
    fn visit(
        root: &std::path::Path,
        path: &std::path::Path,
        out: &mut std::collections::BTreeMap<std::path::PathBuf, Option<Vec<u8>>>,
    ) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let content = if path.is_dir() {
                None
            } else {
                Some(fs::read(&path).unwrap())
            };
            out.insert(path.strip_prefix(root).unwrap().to_owned(), content);
            if path.is_dir() {
                visit(root, &path, out);
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    visit(root, root, &mut out);
    out
}

// 测试两端文件系统均位于本机，显式声明访问方式。
fn local_link(
    remote: Client,
    address: String,
    info: &ServerEndpointInfo,
    local: &LocalSync,
    name: &str,
) -> Result<ServerLink, AppError> {
    let mut link = ServerLink::from_parts(remote, address, info, local, name)?;
    link.set_estimate_file_access(EstimateFileAccess::Local);
    Ok(link)
}

#[cfg(windows)]
#[test]
fn y10_unc_路径无需共享可访问即提示网络位置() {
    for path in [
        r"\\server\share\x",
        r"\\?\UNC\server\share\x",
        "//server/share/x",
    ] {
        let issues = check_local_path(Path::new(path), &[], &LocalEnv::default());
        assert_eq!(
            issues
                .iter()
                .filter(|issue| matches!(issue,
                    PathIssue::CloudFolder { provider } if provider == "network"
                ))
                .count(),
            1,
            "{path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn 真实进程_本机只接收状态还原与编辑回滚() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let Some(bin) = real_bin() else {
        return;
    };
    let root = crate::test_support::tempdir();
    let (local, _) = real_local(&bin, &root.path().join("local-home"));
    let (server, port) = real_local(&bin, &root.path().join("server-home"));
    let info = ServerEndpointInfo {
        server_id: "server".into(),
        name: "服务器".into(),
        device_id: server.device_id().into(),
        socket_path: "/unused".into(),
        sync_port: port,
        api_key: "test-key".into(),
        state_dir: root.path().join("server-state").display().to_string(),
    };
    let mut link = local_link(
        server.client().clone(),
        format!("tcp://127.0.0.1:{port}"),
        &info,
        &local,
        "电脑",
    )
    .unwrap();
    let left = root.path().join("local-files");
    let right = root.path().join("server-files");
    fs::create_dir(&left).unwrap();
    fs::create_dir(&right).unwrap();
    fs::write(right.join("shared.txt"), "服务器原版").unwrap();
    fs::write(right.join("updates.txt"), "服务器初版").unwrap();
    let spec = TaskSpec {
        data_dir: root.path().join("recovery-data"),
        folder_id: None,
        name: "本机只接收".into(),
        local_path: left.clone(),
        local_mode: FolderKind::ReceiveOnly,
        endpoints: vec![EndpointSpec {
            server_id: "server".into(),
            remote_path: right.display().to_string(),
            mode: FolderKind::SendReceive,
        }],
        ignore_rules: vec![],
        ignores_source: IgnoresSource::Unconfirmed,
        watch_delay_s: 10,
        collab: true,
        versioning: false,
        versions_root: root.path().join("versions"),
    };
    let id = create_task(&local, &[&link], &spec).unwrap();
    assert_eq!(
        local.client().get_folder(&id).unwrap().unwrap()["type"],
        "receiveonly"
    );
    let servers = [("server".into(), server.device_id().into())];
    set_paused(&local, &[&link], &id, false).unwrap();
    wait(|| {
        fs::read_to_string(left.join("shared.txt")).is_ok_and(|s| s == "服务器原版")
            && fs::read_to_string(left.join("updates.txt")).is_ok_and(|s| s == "服务器初版")
    });
    fs::write(left.join("shared.txt"), "本机私有改动").unwrap();
    fs::write(left.join("local-only.txt"), "仅本机新增").unwrap();
    rescan(&local, &[&link], &id).unwrap();
    wait(|| task_status(&local, &id, &servers).is_ok_and(|s| s.local_changed_items == 2));
    // 服务器在连接正常时继续发来变化，同时验证本机的改动没有上传。
    fs::write(right.join("updates.txt"), "服务器第二版").unwrap();
    rescan(&local, &[&link], &id).unwrap();
    wait(|| fs::read_to_string(left.join("updates.txt")).is_ok_and(|s| s == "服务器第二版"));
    assert_eq!(
        fs::read_to_string(right.join("shared.txt")).unwrap(),
        "服务器原版"
    );
    assert!(!right.join("local-only.txt").exists());
    assert_eq!(
        task_status(&local, &id, &servers)
            .unwrap()
            .local_changed_items,
        2
    );
    revert_local_changes(&local, &id).unwrap();
    wait(|| {
        task_status(&local, &id, &servers).is_ok_and(|s| s.local_changed_items == 0)
            && fs::read_to_string(left.join("shared.txt")).is_ok_and(|s| s == "服务器原版")
            && !left.join("local-only.txt").exists()
    });
    let mut settings = TaskSettings {
        local_mode: FolderKind::ReceiveOnly,
        endpoints: vec![EndpointSettings {
            server_id: "server".into(),
            mode: FolderKind::SendReceive,
        }],
        watch_delay_s: 19,
        collab: false,
        versioning: true,
        versions_root: spec.versions_root.clone(),
    };
    update_task_settings(&local, &[&link], &id, &settings).unwrap();
    for client in [local.client(), server.client()] {
        let config = client.get_folder(&id).unwrap().unwrap();
        assert_eq!(config["fsWatcherDelayS"], 19);
        assert_eq!(config["versioning"]["type"], "trashcan");
        assert_eq!(config["versioning"]["params"]["cleanoutDays"], "30");
        assert_eq!(config["paused"], false);
    }
    settings.local_mode = FolderKind::SendReceive;
    settings.endpoints[0].mode = FolderKind::ReceiveOnly;
    settings.collab = true;
    settings.versioning = false;
    update_task_settings(&local, &[&link], &id, &settings).unwrap();
    assert_eq!(
        local.client().get_folder(&id).unwrap().unwrap()["type"],
        "sendreceive"
    );
    assert_eq!(
        server.client().get_folder(&id).unwrap().unwrap()["type"],
        "receiveonly"
    );
    for client in [local.client(), server.client()] {
        let config = client.get_folder(&id).unwrap().unwrap();
        assert_eq!(config["fsWatcherDelayS"], 1);
        assert_eq!(config["versioning"]["type"], "");
    }
    set_paused(&local, &[&link], &id, true).unwrap();
    let before_local = local.client().get_folder(&id).unwrap().unwrap();
    let before_server = server.client().get_folder(&id).unwrap().unwrap();
    let original = server.client().clone();
    let fail_once = Arc::new(AtomicBool::new(true));
    let mut failure_client = original.clone();
    failure_client.test_handler = Some(Arc::new(move |method, path, query, body| {
        let result = original.call(method, path, query, body)?;
        if method == "PUT" && fail_once.swap(false, Ordering::Relaxed) {
            // 模拟远端已修改成功、响应途中断开；失败端自身也要回滚。
            return Err(AppError::new("syncthing.unreachable"));
        }
        Ok(result)
    }));
    link.remote = failure_client;
    settings.collab = false;
    settings.watch_delay_s = 41;
    settings.versioning = true;
    let failed = update_task_settings(&local, &[&link], &id, &settings).unwrap_err();
    assert_eq!(failed.code, "syncthing.unreachable");
    assert_eq!(failed.params["endpoint"], "server");
    assert_eq!(
        local.client().get_folder(&id).unwrap().unwrap(),
        before_local
    );
    assert_eq!(
        server.client().get_folder(&id).unwrap().unwrap(),
        before_server
    );
    let mut offline = server.client().clone();
    offline.test_handler = Some(Arc::new(|_, _, _, _| {
        Err(AppError::new("syncthing.unreachable"))
    }));
    link.remote = offline;
    let failed = update_task_settings(&local, &[&link], &id, &settings).unwrap_err();
    assert_eq!(failed.code, "sync.server_offline");
    assert_eq!(failed.params["server_id"], "server");
    assert_eq!(
        local.client().get_folder(&id).unwrap().unwrap(),
        before_local
    );
    assert_eq!(
        server.client().get_folder(&id).unwrap().unwrap(),
        before_server
    );
    drop(link);
    local.stop().unwrap();
    server.stop().unwrap();
}
