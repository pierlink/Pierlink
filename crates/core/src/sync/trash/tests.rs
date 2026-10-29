use super::*;
use serde_json::json;
use std::sync::Arc;

const ID: &str = "pl-1234567890123456";
const DEVICE: &str = "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA";
fn client(path: &Path) -> Client {
    let folder = path.to_string_lossy().into_owned();
    let mut client = Client::new("http://127.0.0.1:1", "测试密钥").unwrap();
    client.test_handler = Some(Arc::new(move |_, path, _, _| match path {
        "/rest/config/folders" => Ok(json!([{"id":ID,"path":folder,"paused":false}])),
        "/rest/system/status" => Ok(json!({"myID": DEVICE})),
        "/rest/config/devices" => Ok(json!([])),
        _ if path.starts_with("/rest/config/devices/") => Ok(json!({})),
        _ => panic!("未预期接口 {path}"),
    }));
    client
}
fn local(path: &Path) -> LocalSync {
    LocalSync {
        instance: None,
        client: client(path),
        id: DEVICE.into(),
        address_updates: Default::default(),
    }
}
fn link(local: &LocalSync, state: &Path, folder: &Path) -> ServerLink {
    ServerLink::from_parts(
        client(folder),
        "tcp://127.0.0.1:12345".into(),
        &super::super::ServerEndpointInfo {
            server_id: "server".into(),
            name: "服务器".into(),
            device_id: DEVICE.into(),
            socket_path: "/state/api.sock".into(),
            sync_port: 23456,
            api_key: "测试密钥".into(),
            state_dir: state.to_string_lossy().into_owned(),
        },
        local,
        "本机",
    )
    .unwrap()
}
fn populate(root: &Path) {
    fs::create_dir_all(root.join(ID).join("nested")).unwrap();
    fs::write(root.join(ID).join(".hidden"), b"123").unwrap();
    fs::write(root.join(ID).join("nested/file"), b"4567").unwrap();
    fs::create_dir_all(root.join("pl-other")).unwrap();
    fs::write(root.join("pl-other/keep"), b"keep").unwrap();
}
#[test]
fn 本机查询清空隔离任务且不要求暂停() {
    let temp = crate::test_support::tempdir();
    let root = temp.path().join("versions");
    let folder = temp.path().join("project");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("keep"), "保持").unwrap();
    let local = local(&folder);
    assert_eq!(
        clear_local_trash(&local, &root, ID).unwrap(),
        TrashUsage::default()
    );
    populate(&root);
    assert_eq!(
        query_trash(&local, &root, &[], ID).unwrap()[0].usage,
        Some(TrashUsage { files: 2, bytes: 7 })
    );
    assert_eq!(
        clear_local_trash(&local, &root, ID).unwrap(),
        TrashUsage { files: 2, bytes: 7 }
    );
    assert!(root.join(ID).is_dir());
    assert!(root.join("pl-other/keep").is_file());
    assert!(folder.join("keep").is_file());
    assert_eq!(
        clear_local_trash(&local, &root, ID).unwrap(),
        TrashUsage::default()
    );
    assert!(clear_local_trash(&local, &root, "../project").is_err());
    assert!(query_trash(&local, &root, &[], "bad").is_err());
    assert_eq!(
        clear_local_trash(&local, &root, "pl-abcdefghijklmnop")
            .unwrap_err()
            .code,
        "sync.folder_not_found"
    );
    let unsafe_local = local_usage(&root, ID, &[root.join(ID).display().to_string()], true);
    assert_eq!(unsafe_local.unwrap_err().code, "sync.trash_unsafe_path");
}
#[cfg(unix)]
#[test]
fn 符号链接祖先版本目录与子目录均不能越界() {
    use std::os::unix::fs::symlink;
    let temp = crate::test_support::tempdir();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "保持").unwrap();
    let root = temp.path().join("versions");
    fs::create_dir(&root).unwrap();
    symlink(&outside, root.join(ID)).unwrap();
    for clear in [false, true] {
        assert!(local_usage(&root, ID, &[], clear).is_err());
    }
    fs::remove_file(root.join(ID)).unwrap();
    fs::create_dir(root.join(ID)).unwrap();
    symlink(&outside, root.join(ID).join("link")).unwrap();
    assert!(local_usage(&root, ID, &[], true).is_err());
    let alias = temp.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(local_usage(&alias, ID, &[], true).is_err());
    assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "保持");
}
#[cfg(unix)]
fn shell_access() -> EstimateFileAccess {
    EstimateFileAccess::Ssh {
        program: crate::process::SshProgram {
            path: "/bin/sh".into(),
        },
        command: crate::ssh::args::SshCommand {
            args: vec!["-s".into()],
            env: vec![],
            config_text: None,
            builtin: None,
            askpass: None,
        },
    }
}
#[cfg(unix)]
#[test]
fn 远端脚本特殊字符用量清空及查询部分失败() {
    let temp = crate::test_support::tempdir();
    let state = temp.path().join("状态 ' $() ` ; \n");
    let folder = temp.path().join("project");
    fs::create_dir(&folder).unwrap();
    let local = local(&folder);
    let mut link = link(&local, &state, &folder);
    let root = state.join("versions");
    populate(&root);
    fs::write(root.join(ID).join("空 格 ' $() ` ; \n"), b"xy").unwrap();
    let results = query_trash(&local, &temp.path().join("local"), &[&link], ID).unwrap();
    assert_eq!(results[0].usage, Some(TrashUsage::default()));
    assert_eq!(
        results[1].error.as_ref().unwrap().code,
        "sync.trash_transport_missing"
    );
    link.set_estimate_file_access(shell_access());
    let result = query_trash(&local, &temp.path().join("local"), &[&link], ID).unwrap();
    assert_eq!(result[1].usage, Some(TrashUsage { files: 3, bytes: 9 }));
    assert_eq!(
        clear_remote_trash(&link, ID).unwrap(),
        TrashUsage { files: 3, bytes: 9 }
    );
    assert!(root.join(ID).is_dir());
    assert!(root.join("pl-other/keep").exists());
    assert_eq!(
        clear_remote_trash(&link, ID).unwrap(),
        TrashUsage::default()
    );
}
#[cfg(unix)]
#[test]
fn 远端拒绝越界软链接重叠路径及缺失目录() {
    use std::os::unix::fs::symlink;
    let temp = crate::test_support::tempdir();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "保持").unwrap();
    let local = local(&outside);
    let mut link = link(&local, temp.path(), &outside);
    link.set_estimate_file_access(shell_access());
    assert_eq!(
        clear_remote_trash(&link, ID).unwrap(),
        TrashUsage::default()
    );
    let root = temp.path().join("versions");
    fs::create_dir(&root).unwrap();
    symlink(&outside, root.join(ID)).unwrap();
    assert_eq!(
        clear_remote_trash(&link, ID).unwrap_err().code,
        "sync.trash_unsafe_path"
    );
    fs::remove_file(root.join(ID)).unwrap();
    fs::create_dir(root.join(ID)).unwrap();
    symlink(&outside, root.join(ID).join("link")).unwrap();
    assert_eq!(
        clear_remote_trash(&link, ID).unwrap_err().code,
        "sync.trash_unsafe_path"
    );
    assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "保持");
    link.remote = client(Path::new("/"));
    assert_eq!(
        clear_remote_trash(&link, ID).unwrap_err().code,
        "sync.trash_unsafe_path"
    );
    link.remote = client(&root.join(ID));
    assert_eq!(
        clear_remote_trash(&link, ID).unwrap_err().code,
        "sync.trash_unsafe_path"
    );
    assert!(remote_script("/state/../project", ID, &[], true).is_err());
    assert!(remote_script("/", ID, &[], true).is_err());
}

#[cfg(unix)]
#[test]
#[ignore = "需要本机 sshd 和随机监听端口"]
fn 内置后台通道查询并清理服务器回收站() {
    let fixture = crate::builtin_test_support::Fixture::new();
    let state = fixture.sshd.root.path().join("state");
    let folder = fixture.sshd.root.path().join("project");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("keep"), "保留同步文件").unwrap();
    let local = local(&folder);
    let mut link = link(&local, &state, &folder);
    link.set_estimate_file_access(EstimateFileAccess::Ssh {
        program: fixture.program(),
        command: fixture.command(),
    });
    populate(&state.join("versions"));
    let result = query_trash(&local, &state.join("local-versions"), &[&link], ID).unwrap();
    assert_eq!(result[1].usage, Some(TrashUsage { files: 2, bytes: 7 }));
    assert_eq!(
        clear_remote_trash(&link, ID).unwrap(),
        TrashUsage { files: 2, bytes: 7 }
    );
    assert_eq!(
        fs::read_to_string(folder.join("keep")).unwrap(),
        "保留同步文件"
    );
}
