//! 持久化转发的字段校验与隧道转换；表内冲突由存储事务检查。
use super::{Readiness, TunnelForward, TunnelSpec};
use crate::{
    error::AppError,
    ssh::{
        args::{CommonOpts, ForwardTarget, forward_target},
        target::SshTarget,
    },
    storage::{ForwardKind, PortForward},
};

fn invalid(field: &str) -> AppError {
    AppError::new("tunnel.invalid_forward").with("field", field)
}

/// 校验 f 的类型、必填及必空字段、端口和目标主机；返回结果，不回显字段原文。
/// 同一服务器已启用条目间的监听端口冲突由 Db 的写入事务检查。
pub fn validate(f: &PortForward) -> Result<(), AppError> {
    for (field, value) in [
        ("id", &f.id),
        ("server_id", &f.server_id),
        ("name", &f.name),
    ] {
        if value.trim().is_empty() || value.contains('\0') {
            return Err(invalid(field));
        }
    }
    if f.local_port == 0 {
        return Err(invalid("local_port"));
    }
    match f.kind {
        ForwardKind::Local => {
            let host = f
                .remote_host
                .as_ref()
                .ok_or_else(|| invalid("remote_host"))?;
            let port = f
                .remote_port
                .filter(|p| *p != 0)
                .ok_or_else(|| invalid("remote_port"))?;
            forward_target(&ForwardTarget::Host {
                host: host.clone(),
                port,
            })?;
        }
        ForwardKind::Remote => {
            if f.remote_host.is_some() {
                return Err(invalid("remote_host"));
            }
            if f.remote_port.is_none_or(|p| p == 0) {
                return Err(invalid("remote_port"));
            }
        }
        ForwardKind::Dynamic => {
            if f.remote_host.is_some() {
                return Err(invalid("remote_host"));
            }
            if f.remote_port.is_some() {
                return Err(invalid("remote_port"));
            }
        }
    }
    Ok(())
}

/// 用已校验的 f、SSH target 和 opts 返回认证标记就绪的固定端口隧道，ID 为 forward:<id>。
/// 调用方须拒绝自带转发配置的 SSH 别名；此处不读取用户 ssh config。
pub fn to_spec(
    f: &PortForward,
    target: SshTarget,
    opts: CommonOpts,
) -> Result<TunnelSpec, AppError> {
    validate(f)?;
    let forward = match f.kind {
        ForwardKind::Local => TunnelForward::Local {
            local_port: Some(f.local_port),
            remote: ForwardTarget::Host {
                host: f
                    .remote_host
                    .clone()
                    .ok_or_else(|| invalid("remote_host"))?,
                port: f.remote_port.ok_or_else(|| invalid("remote_port"))?,
            },
            replace_on_conflict: false,
        },
        ForwardKind::Remote => TunnelForward::Remote {
            remote_port: f.remote_port.ok_or_else(|| invalid("remote_port"))?,
            local_port: f.local_port,
        },
        ForwardKind::Dynamic => TunnelForward::Dynamic {
            local_port: f.local_port,
        },
    };
    Ok(TunnelSpec {
        builtin: None,
        id: format!("forward:{}", f.id),
        server_id: f.server_id.clone(),
        target,
        options: opts,
        forwards: vec![forward],
        readiness: Readiness::AuthMarker,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stored(kind: ForwardKind) -> PortForward {
        PortForward {
            id: "f".into(),
            server_id: "server".into(),
            name: "转发".into(),
            kind,
            local_port: 18080,
            remote_host: (kind == ForwardKind::Local).then(|| "db.internal".into()),
            remote_port: (kind != ForwardKind::Dynamic).then_some(5432),
            enabled: true,
            sort_order: 0,
        }
    }
    fn target() -> SshTarget {
        SshTarget::Alias {
            alias: "server".into(),
        }
    }
    #[test]
    fn 三个模型转换为固定端口认证标记隧道() {
        for kind in [
            ForwardKind::Local,
            ForwardKind::Remote,
            ForwardKind::Dynamic,
        ] {
            let f = stored(kind);
            validate(&f).unwrap();
            let spec = to_spec(&f, target(), CommonOpts::default()).unwrap();
            assert_eq!(spec.id, "forward:f");
            assert_eq!(spec.server_id, "server");
            assert_eq!(spec.readiness, Readiness::AuthMarker);
            assert_eq!(spec.target, target());
            let entry = super::super::Entry::new(spec).unwrap();
            let command = entry.command().unwrap();
            assert!(command.args.contains(&"LogLevel=VERBOSE".into()));
            assert_eq!(
                entry.ports,
                if kind == ForwardKind::Remote {
                    vec![]
                } else {
                    vec![18080]
                }
            );
            match &entry.spec.forwards[0] {
                TunnelForward::Local {
                    local_port,
                    replace_on_conflict,
                    ..
                } => {
                    assert_eq!(*local_port, Some(18080));
                    assert!(!replace_on_conflict);
                }
                TunnelForward::Remote {
                    remote_port,
                    local_port,
                } => {
                    assert_eq!(*remote_port, 5432);
                    assert_eq!(*local_port, 18080);
                }
                TunnelForward::Dynamic { local_port } => assert_eq!(*local_port, 18080),
            }
        }
    }
    #[test]
    fn 各类型必填必空字段与非法值() {
        for kind in [
            ForwardKind::Local,
            ForwardKind::Remote,
            ForwardKind::Dynamic,
        ] {
            let base = stored(kind);
            for field in ["id", "server_id", "name", "local_port"] {
                let mut f = base.clone();
                match field {
                    "id" => f.id.clear(),
                    "server_id" => f.server_id.clear(),
                    "name" => f.name.clear(),
                    _ => f.local_port = 0,
                }
                assert!(validate(&f).is_err());
                assert!(to_spec(&f, target(), CommonOpts::default()).is_err());
            }
            for host in [
                None,
                Some("127.0.0.1"),
                Some("-x"),
                Some("db host"),
                Some("u@host"),
                Some("db'host"),
            ] {
                let mut f = base.clone();
                f.remote_host = host.map(str::to_owned);
                let valid = match kind {
                    ForwardKind::Local => host == Some("127.0.0.1"),
                    _ => host.is_none(),
                };
                assert_eq!(validate(&f).is_ok(), valid);
            }
            for port in [None, Some(0), Some(65535)] {
                let mut f = base.clone();
                f.remote_port = port;
                let valid = match kind {
                    ForwardKind::Dynamic => port.is_none(),
                    _ => port == Some(65535),
                };
                assert_eq!(validate(&f).is_ok(), valid);
            }
        }
    }
}
