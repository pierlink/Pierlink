//! 内置同步双转发与真实设备恢复；文件随公共测试根目录自动清理。
use super::*;
use crate::{
    builtin::ConnectionClass,
    builtin_test_support::{Fixture, sshd},
    ssh::{args::CommonOpts, target::SshTarget},
    syncthing::{
        client::Client,
        folder::FolderKind,
        home::{self, GuiListen, HomeSpec},
        process::{self, SyncthingBin},
    },
    tunnel::{TunnelManager, TunnelState, allocate_port},
};
use std::{
    fs, thread,
    time::{Duration, Instant},
};

fn target(f: &Fixture) -> SshTarget {
    SshTarget::Manual {
        host: "127.0.0.1".into(),
        port: f.sshd.port,
        user: Some(f.sshd.user.clone()),
        identity_file: Some(f.sshd.root.path().join("client")),
        jump: None,
    }
}

fn wait(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "同步未在时限内恢复");
        thread::sleep(Duration::from_millis(50));
    }
}

fn connected(local: &LocalSync, remote: &Client, id: &str) -> bool {
    local.client().connections().unwrap()["connections"][id]["connected"] == true
        && remote.connections().unwrap()["connections"][local.device_id()]["connected"] == true
}

#[test]
#[ignore = "需要普通用户本机 sshd、随机端口及 PIERLINK_TEST_SYNCTHING"]
fn 内置同步双向传输连续断线快速恢复暂停及管理套接字恢复() {
    let bin = SyncthingBin {
        path: std::env::var_os("PIERLINK_TEST_SYNCTHING")
            .expect("必须指定真实 Syncthing 程序")
            .into(),
    };
    let f = Fixture::new();
    let root = f.sshd.root.path();
    let gui = allocate_port().unwrap();
    let mut port = allocate_port().unwrap();
    while port == gui {
        port = allocate_port().unwrap();
    }
    let local = LocalSync::start(
        &bin,
        &root.join("local"),
        gui,
        port,
        "local-key",
        "test-password",
    )
    .unwrap();
    let remote_spec = HomeSpec {
        home: root.join("remote"),
        gui: GuiListen::Unix {
            path: root.join("s/api"),
        },
        gui_user: "pierlink".into(),
        listen_port: allocate_port().unwrap(),
    };
    let remote_info = home::init_home(&bin, &remote_spec, "test-password").unwrap();
    let remote = process::start(&bin, &remote_spec.home, &remote_spec.gui, "remote-key").unwrap();
    let info = ServerEndpointInfo {
        server_id: f.server.id.clone(),
        name: "同步服务器".into(),
        device_id: remote_info.device_id,
        socket_path: root.join("s/api").display().to_string(),
        sync_port: remote_spec.listen_port,
        api_key: "remote-key".into(),
        state_dir: root.join("state").display().to_string(),
    };
    let tunnels = TunnelManager::new(f.program()).unwrap();
    let command = f.command();
    let context = command.builtin.as_ref().unwrap();
    let mut request = context.request();
    request.interactive = true;
    let mut link = ServerLink::connect_with_connection(
        &tunnels,
        target(&f),
        CommonOpts::default(),
        &info,
        &local,
        "电脑",
        &f.manager,
        request,
    )
    .unwrap();
    assert!(
        matches!(&link.estimate_files, Some(EstimateFileAccess::Ssh { command, .. }) if command.builtin.is_some())
    );
    let main = f.manager.get_connection(context.request()).unwrap();
    let bulk = || {
        let mut request = context.request();
        request.class = ConnectionClass::Bulk;
        f.manager.get_connection(request).unwrap()
    };
    assert!(!main.same_connection(&bulk()));
    assert_eq!(bulk().window_size(), 2 * 1024 * 1024);
    let local_files = root.join("本机文件");
    let remote_files = root.join("服务器 ' $() 文件");
    fs::create_dir(&local_files).unwrap();
    fs::create_dir(&remote_files).unwrap();
    fs::create_dir(local_files.join(".git")).unwrap();
    fs::write(local_files.join(".git/config"), "不应同步").unwrap();
    fs::write(local_files.join("本机.txt"), "本机内容").unwrap();
    fs::write(remote_files.join("远端.txt"), "远端内容").unwrap();
    let folder = create_task(
        &local,
        &[&link],
        &TaskSpec {
            data_dir: root.join("data"),
            folder_id: None,
            name: "同步测试".into(),
            local_path: local_files.clone(),
            local_mode: FolderKind::SendReceive,
            endpoints: vec![EndpointSpec {
                server_id: info.server_id.clone(),
                remote_path: remote_files.display().to_string(),
                mode: FolderKind::SendReceive,
            }],
            ignore_rules: crate::syncthing::folder::default_ignores(),
            ignores_source: IgnoresSource::Unconfirmed,
            watch_delay_s: 1,
            collab: false,
            versioning: false,
            versions_root: root.join("versions"),
        },
    )
    .unwrap();
    assert_eq!(
        local.client().get_folder(&folder).unwrap().unwrap()["paused"],
        true
    );
    set_paused(&local, &[&link], &folder, false).unwrap();
    wait(Duration::from_secs(30), || {
        connected(&local, remote.client(), &info.device_id)
            && local_files.join("远端.txt").exists()
            && remote_files.join("本机.txt").exists()
    });
    assert_eq!(
        fs::read_to_string(local_files.join("远端.txt")).unwrap(),
        "远端内容"
    );
    assert_eq!(
        fs::read_to_string(remote_files.join("本机.txt")).unwrap(),
        "本机内容"
    );
    assert!(!remote_files.join(".git").exists());
    let id = format!("sync:{}", info.server_id);
    let original = tunnels.control().status(&id).unwrap();
    for round in 0..5 {
        let before = tunnels.control().status(&id).unwrap().last_connected_at_ms;
        let old = bulk();
        old.abort();
        tunnels.network_restored().unwrap();
        let started = Instant::now();
        wait(Duration::from_secs(8), || {
            let status = tunnels.control().status(&id).unwrap();
            status.state == TunnelState::Connected
                && status.last_connected_at_ms != before
                && connected(&local, remote.client(), &info.device_id)
        });
        eprintln!(
            "内置同步第 {} 次恢复耗时：{:?}",
            round + 1,
            started.elapsed()
        );
        assert!(!old.same_connection(&bulk()));
        assert!(!main.is_closed());
        assert_eq!(
            tunnels.control().status(&id).unwrap().local_ports,
            original.local_ports
        );
    }
    link.refresh_address(&local).unwrap();
    let before = tunnels.control().status(&id).unwrap().last_connected_at_ms;
    tunnels.network_restored().unwrap();
    thread::sleep(Duration::from_millis(700));
    assert_eq!(
        tunnels.control().status(&id).unwrap().last_connected_at_ms,
        before
    );
    set_paused(&local, &[&link], &folder, true).unwrap();
    fs::write(local_files.join("恢复后.txt"), "恢复后内容").unwrap();
    tunnels.reconnect_all().unwrap();
    wait(Duration::from_secs(8), || {
        tunnels.control().status(&id).unwrap().last_connected_at_ms != before
    });
    assert_eq!(
        local.client().get_folder(&folder).unwrap().unwrap()["paused"],
        true
    );
    assert_eq!(
        remote.client().get_folder(&folder).unwrap().unwrap()["paused"],
        true
    );
    assert!(!remote_files.join("恢复后.txt").exists());
    set_paused(&local, &[&link], &folder, false).unwrap();
    wait(Duration::from_secs(30), || {
        remote_files.join("恢复后.txt").exists()
    });
    assert_eq!(
        fs::read_to_string(remote_files.join("恢复后.txt")).unwrap(),
        "恢复后内容"
    );
    // 管理套接字暂时缺失仍按原有探活退避重建，不能误判为永久禁止转发。
    let before = tunnels.control().status(&id).unwrap().last_connected_at_ms;
    fs::rename(root.join("s/api"), root.join("s/offline")).unwrap();
    tunnels.network_restored().unwrap();
    wait(Duration::from_secs(10), || {
        let status = tunnels.control().status(&id).unwrap();
        assert_ne!(status.state, TunnelState::Error);
        status.state == TunnelState::Reconnecting || status.last_connected_at_ms != before
    });
    fs::rename(root.join("s/offline"), root.join("s/api")).unwrap();
    wait(Duration::from_secs(10), || {
        let status = tunnels.control().status(&id).unwrap();
        status.state == TunnelState::Connected
            && status.last_connected_at_ms != before
            && connected(&local, remote.client(), &info.device_id)
    });
    link.refresh_address(&local).unwrap();
    remove_task(&local, &[&link], &folder).unwrap();
    assert!(local_files.join("本机.txt").exists() && remote_files.join("远端.txt").exists());
    assert_eq!(
        tunnels.control().status(&id).unwrap_err().code,
        "tunnel.not_found"
    );
    assert!(!main.is_closed() && !bulk().is_closed());
    link.disconnect(&tunnels).unwrap();
    remote.stop().unwrap();
    local.stop().unwrap();
}

#[test]
#[ignore = "需要普通用户本机 sshd 和随机监听端口"]
fn 内置同步禁止套接字转发返回明确错误并回收双监听() {
    let sshd = sshd::Server::with_root(
        crate::test_support::tempdir(),
        "AllowStreamLocalForwarding no",
        true,
    );
    let f = Fixture::from_sshd(sshd);
    let tunnels = TunnelManager::new(f.program()).unwrap();
    let local = LocalSync {
        instance: None,
        client: Client::new("http://127.0.0.1:1", "test-key").unwrap(),
        id: "AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA-AAAAAAA".into(),
        address_updates: Default::default(),
    };
    let info = ServerEndpointInfo {
        server_id: f.server.id.clone(),
        name: "服务器".into(),
        device_id: local.device_id().into(),
        socket_path: "/private/api.sock".into(),
        sync_port: 22000,
        api_key: "test-key".into(),
        state_dir: "/state".into(),
    };
    let command = f.command();
    let started = Instant::now();
    let error = ServerLink::connect_with_connection(
        &tunnels,
        target(&f),
        CommonOpts::default(),
        &info,
        &local,
        "电脑",
        &f.manager,
        command.builtin.as_ref().unwrap().request(),
    )
    .err()
    .unwrap();
    assert_eq!(error.code, "ssh.streamlocal_open_failed");
    // 部分 sshd 用目标不可达回复禁止套接字转发，须保留启动宽限后仍给出具体原因。
    assert!(started.elapsed() < Duration::from_secs(35));
    assert!(error.params["reason"].contains("服务器禁止"));
    assert_eq!(
        tunnels
            .control()
            .status(&format!("sync:{}", info.server_id))
            .unwrap_err()
            .code,
        "tunnel.not_found"
    );
    let connection = f
        .manager
        .get_connection(command.builtin.as_ref().unwrap().request())
        .unwrap();
    assert_eq!(sshd::command(&connection, "printf terminal"), b"terminal");
}
