//! 服务器偏好与功能开关共同决定连接方式；新增内置功能时只开启对应项。
pub use crate::storage::Engine;
use crate::storage::{Server, ServerSource};

/// 可独立切换到内置连接的功能。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Background,
    Terminal,
    Sftp,
    PortForward,
    SyncTunnel,
    DeployUpload,
}

/// 内置功能开关表；各功能仍遵守服务器的引擎偏好与参数回退规则。
pub const FEATURE_SUPPORT: &[(Feature, bool)] = &[
    (Feature::Background, true),
    (Feature::Terminal, true),
    (Feature::Sftp, true),
    (Feature::PortForward, true),
    (Feature::SyncTunnel, true),
    (Feature::DeployUpload, true),
];

/// 一次选择及可展示的固定中文回退原因；显式选择 OpenSSH 时不附回退原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSelection {
    pub engine: Engine,
    pub reason: Option<&'static str>,
}

fn openssh(reason: &'static str) -> EngineSelection {
    EngineSelection {
        engine: Engine::OpenSsh,
        reason: Some(reason),
    }
}

/// 按 server 的偏好与连接参数返回可用引擎及回退原因，不建立连接或读取 ssh config。
/// 功能调用方必须再使用 engine_for_feature 检查功能开关。
pub fn select_engine(server: &Server) -> EngineSelection {
    if server.connection_engine == Engine::OpenSsh {
        return EngineSelection {
            engine: Engine::OpenSsh,
            reason: None,
        };
    }
    if server.ssh_alias.is_some() || server.source == ServerSource::SshConfig {
        return openssh("服务器仍依赖 ssh config 别名，需按原配置连接");
    }
    if server.host.as_deref().is_none_or(|s| s.trim().is_empty()) {
        return openssh("服务器未填写主机地址，需由系统 OpenSSH 解析");
    }
    if server
        .username
        .as_deref()
        .is_none_or(|s| s.trim().is_empty())
    {
        return openssh("服务器未填写用户名，需由系统 OpenSSH 确定登录用户");
    }
    EngineSelection {
        engine: Engine::Builtin,
        reason: None,
    }
}

/// 查询 feature 是否已接入内置引擎；未列入开关表的功能默认关闭。
pub fn supports_builtin(feature: Feature) -> bool {
    FEATURE_SUPPORT
        .iter()
        .any(|&(entry, supported)| entry == feature && supported)
}

/// 返回 server 执行 feature 时的实际引擎及原因；功能未接入时使用系统 OpenSSH。
pub fn engine_for_feature(server: &Server, feature: Feature) -> EngineSelection {
    let selected = select_engine(server);
    if selected.engine == Engine::Builtin && !supports_builtin(feature) {
        openssh("该功能尚未接入内置 SSH，暂用系统 OpenSSH")
    } else {
        selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        let mut server = crate::server_input::build_alias_server("dev", None, "s", 0).unwrap();
        server.source = ServerSource::Manual;
        server.ssh_alias = None;
        server.host = Some("example.test".into());
        server.username = Some("dev".into());
        server.connection_engine = Engine::Builtin;
        server
    }

    #[test]
    fn 显式系统模式与完整内置记录() {
        let mut server = server();
        assert_eq!(select_engine(&server).engine, Engine::Builtin);
        assert_eq!(select_engine(&server).reason, None);
        server.jump_server_id = Some("jump".into());
        assert_eq!(select_engine(&server).engine, Engine::Builtin);
        server.connection_engine = Engine::OpenSsh;
        server.host = None;
        assert_eq!(select_engine(&server).engine, Engine::OpenSsh);
        assert_eq!(select_engine(&server).reason, None);
    }

    #[test]
    fn 依赖别名或缺少连接参数时说明回退原因() {
        let base = server();
        let mut cases = Vec::new();
        let mut alias = base.clone();
        alias.ssh_alias = Some("dev".into());
        cases.push(alias);
        let mut imported = base.clone();
        imported.source = ServerSource::SshConfig;
        cases.push(imported);
        for value in [None, Some("".into()), Some("  ".into())] {
            let mut host = base.clone();
            host.host = value.clone();
            cases.push(host);
            let mut user = base.clone();
            user.username = value;
            cases.push(user);
        }
        for server in cases {
            let selected = select_engine(&server);
            assert_eq!(selected.engine, Engine::OpenSsh);
            assert!(selected.reason.is_some_and(|s| !s.is_empty()));
        }
    }

    #[test]
    fn 已接入功能启用内置且不会覆盖已有回退原因() {
        let mut server = server();
        assert_eq!(FEATURE_SUPPORT.len(), 6);
        for (index, &(feature, enabled)) in FEATURE_SUPPORT.iter().enumerate() {
            assert!(!FEATURE_SUPPORT[..index].iter().any(|&(f, _)| f == feature));
            assert_eq!(
                enabled,
                matches!(
                    feature,
                    Feature::Background
                        | Feature::Terminal
                        | Feature::Sftp
                        | Feature::PortForward
                        | Feature::SyncTunnel
                        | Feature::DeployUpload
                )
            );
            assert_eq!(supports_builtin(feature), enabled);
            let actual = engine_for_feature(&server, feature);
            assert_eq!(
                actual.engine,
                if enabled {
                    Engine::Builtin
                } else {
                    Engine::OpenSsh
                }
            );
            assert_eq!(actual.reason.is_some(), !enabled);
        }
        server.host = None;
        assert_eq!(
            engine_for_feature(&server, Feature::Sftp),
            select_engine(&server)
        );
        server.connection_engine = Engine::OpenSsh;
        assert_eq!(engine_for_feature(&server, Feature::Terminal).reason, None);
    }
}
