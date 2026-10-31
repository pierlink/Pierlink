//! 引擎错误只映射受控类别；不转发自由文本、文件内容、路径或认证回答。
use crate::error::AppError;
use pierlink_ssh_engine::{ChannelFailure, Error};

/// 通道操作上下文；OpenSSH 对 MaxSessions 与套接字转发失败的回复不能单凭错误区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Other,
    OpenSession,
    OpenForward,
    OpenStreamLocal,
}

/// detail 是界面按当前语言翻译的说明的键（`errors.detail.<detail>`），翻译后填进现有译文的
/// stderr_tail / reason 占位符；reason 是没有翻译时显示的中文兜底。两者都是固定文字，不是服务端原文。
fn failure(code: &str, detail: &'static str, reason: &'static str) -> AppError {
    AppError::new(code)
        .with_text("reason", detail, reason)
        .with_text("stderr_tail", detail, reason)
}

/// 无参数；返回服务器拒绝带提示符钩子的登录命令的固定说明。
pub(crate) fn hook_rejected() -> AppError {
    failure(
        "ssh.failed",
        "hook_rejected",
        "服务器不接受带提示符钩子的登录命令",
    )
}

/// 登录途中（建连、换钥、认证）连接断了：服务器的登录时限到了或网络断了。
/// 和用了一阵之后的断线分开提示；错误码仍是 ssh.unreachable，重试和等待的处理不变。
pub fn login_disconnected() -> AppError {
    failure(
        "ssh.unreachable",
        "disconnected_login",
        "登录过程中连接断开了（可能是服务器的登录时限到了或网络中断），请重新连接",
    )
}

/// 将 error 按 operation 映射为前端 AppError；参数仅含固定文字的键、固定中文或数值拒绝码。
/// HostKeyRejected 无法区分首次拒绝与指纹变化，保守映射为指纹不符；主机密钥回调另行区分首次确认。
/// ConnectFailed 只提示可能的会话限制或转发限制，不能断言服务器的具体配置。
pub fn map_engine_error(error: &Error, operation: Operation) -> AppError {
    match error {
        Error::InvalidInput => AppError::new("ssh.invalid_argument").with_text(
            "field",
            "builtin.connection_parameters",
            "连接参数",
        ),
        Error::AsyncContext | Error::Runtime => failure(
            "ssh.task_failed",
            "runtime",
            "内置 SSH 运行时或回调任务执行失败",
        ),
        Error::Dns => failure(
            "ssh.unreachable",
            "dns",
            "域名解析失败，请检查服务器地址和网络",
        ),
        Error::Network(_) => failure(
            "ssh.unreachable",
            "network",
            "网络连接失败，请检查服务器地址、端口和网络",
        ),
        Error::Timeout(_) => failure(
            "ssh.unreachable",
            "timeout",
            "SSH 操作超时，请检查网络后重试",
        ),
        Error::HostKeyRejected => AppError::new("ssh.host_key_changed"),
        Error::Authentication => AppError::new("ssh.auth_failed"),
        Error::TooManyAuthAttempts => AppError::new("ssh.too_many_auth_attempts"),
        Error::PrivateKey => failure(
            "ssh.auth_failed",
            "private_key",
            "无法读取或解密私钥，请检查文件和口令",
        ),
        Error::Callback => AppError::new("ssh.auth_cancelled"),
        Error::Agent => failure(
            "ssh.auth_failed",
            "agent",
            "SSH agent 不可用或拒绝签名，请检查服务和已加载密钥",
        ),
        Error::Algorithm => failure(
            "ssh.failed",
            "algorithm",
            "服务器与内置 SSH 没有共同的签名算法，可改用系统 OpenSSH",
        ),
        Error::ChannelRejected(reason) => channel_error(*reason, operation),
        Error::RequestRejected("sftp") => AppError::new("sftp.unavailable"),
        Error::RequestRejected(_) => {
            failure("ssh.failed", "request_rejected", "服务器拒绝了 SSH 请求")
        }
        Error::Disconnected => failure(
            "ssh.unreachable",
            "disconnected",
            "SSH 连接或通道已关闭，请重新连接",
        ),
        Error::Protocol => failure("ssh.failed", "protocol", "SSH 或子系统协议异常"),
    }
}

fn channel_error(reason: ChannelFailure, operation: Operation) -> AppError {
    match (reason, operation) {
        (ChannelFailure::Prohibited, Operation::OpenForward) => failure(
            "ssh.forwarding_prohibited",
            "forwarding_prohibited",
            "服务器禁止了此类转发",
        ),
        (
            ChannelFailure::ConnectFailed | ChannelFailure::ResourceShortage,
            Operation::OpenSession,
        ) => failure(
            "ssh.session_open_failed",
            "session_limit",
            "无法打开会话，可能达到服务器 MaxSessions 限制或资源不足",
        ),
        (
            ChannelFailure::Prohibited
            | ChannelFailure::ConnectFailed
            | ChannelFailure::UnknownType,
            Operation::OpenStreamLocal,
        ) => failure(
            "ssh.streamlocal_open_failed",
            "streamlocal",
            "无法打开套接字转发，可能被服务器禁止，或套接字不存在、无权限",
        ),
        (ChannelFailure::ConnectFailed, Operation::OpenForward) => failure(
            "ssh.forward_open_failed",
            "forward_target",
            "无法打开转发，可能目标不可达或服务器限制了转发",
        ),
        (ChannelFailure::Other(code), _) => {
            failure("ssh.failed", "channel_rejected", "服务器拒绝了 SSH 通道")
                .with("channel_code", code)
        }
        (ChannelFailure::Prohibited, _) => {
            failure("ssh.failed", "channel_prohibited", "服务器禁止了此类通道")
        }
        (ChannelFailure::UnknownType, _) => {
            failure("ssh.failed", "channel_unknown", "服务器不支持此类通道")
        }
        (ChannelFailure::ResourceShortage, _) => failure(
            "ssh.failed",
            "channel_resources",
            "服务器没有足够资源打开通道",
        ),
        (ChannelFailure::ConnectFailed, _) => {
            failure("ssh.failed", "channel_failed", "无法打开 SSH 通道")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 每条固定说明都有中英文翻译() {
        let locales = [
            include_str!("../../../../src/locales/zh-CN.json"),
            include_str!("../../../../src/locales/en.json"),
        ]
        .map(|text| serde_json::from_str::<serde_json::Value>(text).unwrap());
        for locale in &locales {
            assert!(
                locale["errors"]["ssh"]["too_many_auth_attempts"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
            );
        }
        let mut errors = vec![login_disconnected(), hook_rejected()];
        for error in [
            Error::AsyncContext,
            Error::Dns,
            Error::Network(std::io::ErrorKind::ConnectionRefused),
            Error::Timeout("连接"),
            Error::PrivateKey,
            Error::Agent,
            Error::Algorithm,
            Error::RequestRejected("请求"),
            Error::Disconnected,
            Error::Protocol,
        ] {
            errors.push(map_engine_error(&error, Operation::Other));
        }
        for operation in [
            Operation::Other,
            Operation::OpenSession,
            Operation::OpenForward,
            Operation::OpenStreamLocal,
        ] {
            for reason in [
                ChannelFailure::Prohibited,
                ChannelFailure::ConnectFailed,
                ChannelFailure::UnknownType,
                ChannelFailure::ResourceShortage,
                ChannelFailure::Other(7),
            ] {
                errors.push(map_engine_error(&Error::ChannelRejected(reason), operation));
            }
        }
        for error in errors {
            let detail = &error.params["reason_key"];
            assert_eq!(detail, &error.params["stderr_tail_key"]);
            assert!(!error.params.contains_key("detail"));
            for locale in &locales {
                assert!(
                    locale["errors"]["detail"][detail]
                        .as_str()
                        .is_some_and(|text| !text.is_empty()),
                    "缺少翻译: {detail}"
                );
            }
        }
    }

    #[test]
    fn 每类引擎错误均映射为稳定错误码() {
        let cases = [
            (Error::InvalidInput, "ssh.invalid_argument"),
            (Error::AsyncContext, "ssh.task_failed"),
            (Error::Runtime, "ssh.task_failed"),
            (Error::Dns, "ssh.unreachable"),
            (
                Error::Network(std::io::ErrorKind::ConnectionRefused),
                "ssh.unreachable",
            ),
            (Error::Timeout("连接"), "ssh.unreachable"),
            (Error::HostKeyRejected, "ssh.host_key_changed"),
            (Error::Authentication, "ssh.auth_failed"),
            (Error::TooManyAuthAttempts, "ssh.too_many_auth_attempts"),
            (Error::PrivateKey, "ssh.auth_failed"),
            (Error::Callback, "ssh.auth_cancelled"),
            (Error::Agent, "ssh.auth_failed"),
            (Error::Algorithm, "ssh.failed"),
            (
                Error::ChannelRejected(ChannelFailure::ConnectFailed),
                "ssh.failed",
            ),
            (Error::RequestRejected("请求"), "ssh.failed"),
            (Error::RequestRejected("sftp"), "sftp.unavailable"),
            (Error::Disconnected, "ssh.unreachable"),
            (Error::Protocol, "ssh.failed"),
        ];
        for (error, code) in cases {
            assert_eq!(
                map_engine_error(&error, Operation::Other).code,
                code,
                "{error:?}"
            );
        }
    }

    #[test]
    fn 会话上限与转发失败按上下文提示且保留不确定性() {
        for (operation, expected) in [
            (Operation::OpenSession, "ssh.session_open_failed"),
            (Operation::OpenForward, "ssh.forward_open_failed"),
            (Operation::OpenStreamLocal, "ssh.streamlocal_open_failed"),
            (Operation::Other, "ssh.failed"),
        ] {
            let error = map_engine_error(
                &Error::ChannelRejected(ChannelFailure::ConnectFailed),
                operation,
            );
            assert_eq!(error.code, expected);
            if operation != Operation::Other {
                assert!(error.params["reason"].contains("可能"));
            }
            for reason in [
                ChannelFailure::Prohibited,
                ChannelFailure::UnknownType,
                ChannelFailure::ResourceShortage,
                ChannelFailure::Other(123),
            ] {
                let mapped = map_engine_error(&Error::ChannelRejected(reason), operation);
                assert!(!mapped.code.is_empty());
                if reason == ChannelFailure::Prohibited && operation == Operation::OpenForward {
                    assert_eq!(mapped.code, "ssh.forwarding_prohibited");
                }
                if operation == Operation::OpenStreamLocal
                    && matches!(
                        reason,
                        ChannelFailure::Prohibited | ChannelFailure::UnknownType
                    )
                {
                    assert_eq!(mapped.code, "ssh.streamlocal_open_failed");
                }
            }
        }
        assert_eq!(
            map_engine_error(
                &Error::ChannelRejected(ChannelFailure::ResourceShortage),
                Operation::OpenSession
            )
            .code,
            "ssh.session_open_failed"
        );
    }

    #[test]
    fn 复用指纹网络及认证错误且不泄露秘密() {
        const SECRET: &str =
            "password=秘密口令 api_key=秘密密钥 -----BEGIN OPENSSH PRIVATE KEY----- 服务器原文";
        for error in [Error::Timeout(SECRET), Error::RequestRejected(SECRET)] {
            for operation in [
                Operation::Other,
                Operation::OpenSession,
                Operation::OpenForward,
                Operation::OpenStreamLocal,
            ] {
                let mapped = map_engine_error(&error, operation);
                let text = format!(
                    "{mapped:?} {mapped} {}",
                    serde_json::to_string(&mapped).unwrap()
                );
                for secret in [SECRET, "秘密口令", "秘密密钥", "PRIVATE KEY", "服务器原文"]
                {
                    assert!(!text.contains(secret));
                }
            }
        }
        let mapped = map_engine_error(
            &Error::ChannelRejected(ChannelFailure::Other(99)),
            Operation::Other,
        );
        assert_eq!(mapped.params["channel_code"], "99");
    }
}
