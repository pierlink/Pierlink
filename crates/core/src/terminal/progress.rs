//! 连接进度只向前推进，重试和跳板不能倒退或重复通知。
use super::*;

/// 每个值表示进入此阶段；Ready 表示终端会话已可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectStage {
    Resolve,
    Connect,
    Auth,
    Session,
    Ready,
}

/// 连接阶段回调；只传阶段，不传主机、密码等数据，调用方必须快速返回。
pub type ConnectProgress = Arc<dyn Fn(ConnectStage) + Send + Sync>;

pub(crate) fn ordered(callback: ConnectProgress) -> ConnectProgress {
    let last = Mutex::new(None);
    Arc::new(move |stage| {
        let mut previous = lock(&last);
        if previous.is_some_and(|last| stage <= last) {
            return;
        }
        *previous = Some(stage);
        drop(previous);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(stage)));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 阶段只前进且按蛇形名称序列化() {
        let stages = Arc::new(Mutex::new(Vec::new()));
        let observed = stages.clone();
        let report = ordered(Arc::new(move |stage| lock(&observed).push(stage)));
        for stage in [
            ConnectStage::Session,
            ConnectStage::Resolve,
            ConnectStage::Session,
            ConnectStage::Ready,
            ConnectStage::Auth,
        ] {
            report(stage);
        }
        assert_eq!(*lock(&stages), [ConnectStage::Session, ConnectStage::Ready]);
        assert_eq!(
            serde_json::to_value([
                ConnectStage::Resolve,
                ConnectStage::Connect,
                ConnectStage::Auth,
                ConnectStage::Session,
                ConnectStage::Ready
            ])
            .unwrap(),
            serde_json::json!(["resolve", "connect", "auth", "session", "ready"])
        );
    }
}
