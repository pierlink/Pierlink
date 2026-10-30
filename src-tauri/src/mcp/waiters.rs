//! 等界面回答：发出请求后阻塞等待界面调用命令回答，带期限和取消。确认弹框、打开 AI 终端都用它。
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

struct Pending<D, T> {
    /// 请求内容（界面重新加载后可以取回还没回答的请求）
    request: D,
    tx: mpsc::SyncSender<T>,
}

/// 一类等待界面回答的请求
pub struct Waiters<D, T> {
    seq: AtomicU64,
    pending: Mutex<HashMap<u64, Pending<D, T>>>,
    stopped: AtomicBool,
}

impl<D, T> Default for Waiters<D, T> {
    fn default() -> Self {
        Self {
            seq: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
        }
    }
}

impl<D: Clone, T> Waiters<D, T> {
    fn lock(&self) -> MutexGuard<'_, HashMap<u64, Pending<D, T>>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 登记一个请求并阻塞等回答。
    ///
    /// :param make: 用分配到的编号生成请求内容
    /// :param send: 把请求发给界面（失败也继续等，界面重新加载后会取回）
    /// :param timeout: 最多等多久
    /// :param cancelled: 调用方放弃等待
    /// :return: 界面的回答；超时、取消、已停止时返回 None（请求编号一并返回，方便通知界面关掉）
    pub fn ask(
        &self,
        make: impl FnOnce(u64) -> D,
        send: impl FnOnce(&D),
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> (u64, Option<T>) {
        let id = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        if self.stopped.load(Ordering::Acquire) {
            return (id, None);
        }
        let request = make(id);
        let (tx, rx) = mpsc::sync_channel(1);
        self.lock().insert(
            id,
            Pending {
                request: request.clone(),
                tx,
            },
        );
        send(&request);
        let deadline = Instant::now() + timeout;
        let answer = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(answer) => break Some(answer),
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline || cancelled.load(Ordering::Acquire) {
                        break None;
                    }
                }
            }
        };
        self.lock().remove(&id);
        (id, answer)
    }

    /// 回答一个请求
    ///
    /// :param id: 请求编号
    /// :param answer: 回答
    /// :return: 找不到（已超时、已回答）时返回 false
    pub fn answer(&self, id: u64, answer: T) -> bool {
        match self.lock().remove(&id) {
            Some(p) => p.tx.try_send(answer).is_ok(),
            None => false,
        }
    }

    /// 还没回答的请求（按编号排序）
    pub fn pending(&self) -> Vec<D> {
        let mut all: Vec<(u64, D)> = self
            .lock()
            .iter()
            .map(|(id, p)| (*id, p.request.clone()))
            .collect();
        all.sort_by_key(|(id, _)| *id);
        all.into_iter().map(|(_, d)| d).collect()
    }

    /// 退出时：放弃全部等待，之后不再登记新请求
    ///
    /// :return: 被放弃的请求编号
    pub fn cancel_all(&self) -> Vec<u64> {
        self.stopped.store(true, Ordering::Release);
        // 发送端随条目一起丢掉，等待的一方立即收到“断开”
        self.lock().drain().map(|(id, _)| id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn 回答后返回_超时返回空() {
        let waiters: Arc<Waiters<String, bool>> = Arc::default();
        let w = waiters.clone();
        let answering = std::thread::spawn(move || {
            for _ in 0..100 {
                if let Some(request) = w.pending().first() {
                    let id: u64 = request.parse().unwrap();
                    assert!(w.answer(id, true));
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("没有等到请求");
        });
        let never = AtomicBool::new(false);
        let (_, answer) = waiters.ask(|id| id.to_string(), |_| {}, Duration::from_secs(5), &never);
        answering.join().unwrap();
        assert_eq!(answer, Some(true));
        assert!(waiters.pending().is_empty());
        let started = Instant::now();
        let (id, answer) = waiters.ask(
            |id| id.to_string(),
            |_| {},
            Duration::from_millis(300),
            &never,
        );
        assert_eq!(answer, None);
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(!waiters.answer(id, true));
    }

    #[test]
    fn 取消和停止都会立即结束等待() {
        let waiters: Arc<Waiters<u64, ()>> = Arc::default();
        let cancelled = AtomicBool::new(true);
        let started = Instant::now();
        assert_eq!(
            waiters
                .ask(|id| id, |_| {}, Duration::from_secs(30), &cancelled)
                .1,
            None
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let w = waiters.clone();
        let waiting = std::thread::spawn(move || {
            w.ask(
                |id| id,
                |_| {},
                Duration::from_secs(30),
                &AtomicBool::new(false),
            )
            .1
        });
        while waiters.pending().is_empty() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(waiters.cancel_all().len(), 1);
        assert_eq!(waiting.join().unwrap(), None);
        assert_eq!(
            waiters
                .ask(
                    |id| id,
                    |_| {},
                    Duration::from_secs(30),
                    &AtomicBool::new(false)
                )
                .1,
            None
        );
    }
}
