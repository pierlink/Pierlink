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
        // 检查与送达共用同一把锁，防止并发回调把较后的阶段先送达。
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(stage)));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 并发阶段必须等前一回调送达后才送达() {
        let (entered, started) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let resume = Mutex::new(resume);
        let (sent, received) = mpsc::channel();
        let report = ordered(Arc::new(move |stage| {
            if stage == ConnectStage::Auth {
                entered.send(()).unwrap();
                lock(&resume).recv_timeout(Duration::from_secs(5)).unwrap();
            }
            sent.send(stage).unwrap();
        }));
        thread::scope(|scope| {
            scope.spawn(|| report(ConnectStage::Auth));
            started.recv_timeout(Duration::from_secs(5)).unwrap();
            let (calling, called) = mpsc::channel();
            let report = &report;
            scope.spawn(move || {
                calling.send(()).unwrap();
                report(ConnectStage::Session);
            });
            called.recv_timeout(Duration::from_secs(5)).unwrap();
            let premature = received.recv_timeout(Duration::from_millis(100));
            // 先释放回调，失败时也保证两个线程都能退出。
            release.send(()).unwrap();
            assert_eq!(premature, Err(mpsc::RecvTimeoutError::Timeout));
        });
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            [ConnectStage::Auth, ConnectStage::Session]
        );
    }

    #[test]
    fn 回调异常后仍能报告后续阶段() {
        let stages = Arc::new(Mutex::new(Vec::new()));
        let observed = stages.clone();
        let report = ordered(Arc::new(move |stage| {
            if stage == ConnectStage::Auth {
                panic!("模拟回调异常");
            }
            lock(&observed).push(stage);
        }));
        report(ConnectStage::Auth);
        report(ConnectStage::Resolve);
        report(ConnectStage::Ready);
        assert_eq!(*lock(&stages), [ConnectStage::Ready]);
    }

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
