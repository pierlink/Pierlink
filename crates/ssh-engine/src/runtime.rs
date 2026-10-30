use crate::{Error, Result};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::runtime::{Builder, Runtime};

thread_local! {
    /// 当前线程是不是引擎运行时起的线程（工作线程和阻塞线程都算）：在这些线程上不能再阻塞等待引擎（会把自己卡死）
    static ENGINE_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) struct Executor(Option<Runtime>);

impl Executor {
    pub(crate) fn new() -> Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pierlink-ssh")
            .on_thread_start(|| ENGINE_THREAD.with(|flag| flag.set(true)))
            .enable_all()
            .build()
            .map_err(|_| Error::Runtime)?;
        Ok(Self(Some(runtime)))
    }

    pub(crate) fn handle(&self) -> Result<&tokio::runtime::Handle> {
        self.0.as_ref().map(Runtime::handle).ok_or(Error::Runtime)
    }

    /// 阻塞等 future 完成。
    /// - 普通线程：直接在本线程等；
    /// - 别的 tokio 运行时里的线程（例如 Tauri 命令用的阻塞线程池）：tokio 不允许在这里直接等，
    ///   改在一个临时线程上等，结果交回调用方；
    /// - 引擎自己运行时的线程：返回 AsyncContext（在这里等会把自己卡死）。
    fn block_on<T: Send>(&self, future: impl Future<Output = Result<T>> + Send) -> Result<T> {
        let handle = self.handle()?;
        if tokio::runtime::Handle::try_current().is_err() {
            return handle.block_on(future);
        }
        if ENGINE_THREAD.with(std::cell::Cell::get) {
            return Err(Error::AsyncContext);
        }
        std::thread::scope(|scope| {
            let worker = std::thread::Builder::new()
                .name("pierlink-ssh-wait".into())
                .spawn_scoped(scope, || handle.block_on(future))
                .map_err(|_| Error::Runtime)?;
            worker.join().unwrap_or(Err(Error::Runtime))
        })
    }

    pub(crate) fn wait<T: Send>(
        &self,
        future: impl Future<Output = Result<T>> + Send,
    ) -> Result<T> {
        self.block_on(future)
    }

    pub(crate) fn run<T: Send>(
        &self,
        duration: Duration,
        operation: &'static str,
        future: impl Future<Output = Result<T>> + Send,
    ) -> Result<T> {
        self.block_on(async {
            tokio::time::timeout(duration, future)
                .await
                .map_err(|_| Error::Timeout(operation))?
        })
    }

    /// 在 duration 内运行 future；cancelled 每 25 毫秒检查，取消返回 Callback。
    pub(crate) fn run_cancellable<T: Send>(
        &self,
        duration: Duration,
        operation: &'static str,
        cancelled: Option<Arc<AtomicBool>>,
        future: impl Future<Output = Result<T>> + Send,
    ) -> Result<T> {
        self.run(duration, operation, async {
            tokio::select! {
                biased;
                _ = async {
                    loop {
                        if cancelled.as_ref().is_some_and(|flag| flag.load(Ordering::Acquire)) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                } => Err(Error::Callback),
                result = future => result,
            }
        })
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // 最后一个引用也可能在异步任务中释放，不能在此阻塞等待工作线程。
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedicated_runtime_times_out_and_rejects_nested_block_on() {
        let runtime = Executor::new().unwrap();
        assert_eq!(
            runtime.run(Duration::from_secs(1), "测试", async { Ok(3) }),
            Ok(3)
        );
        assert_eq!(
            runtime.run(
                Duration::from_millis(1),
                "测试",
                std::future::pending::<Result<()>>()
            ),
            Err(Error::Timeout("测试"))
        );
        // 引擎自己运行时的线程上等引擎会把自己卡死：拒绝
        let inner = std::sync::Arc::new(runtime);
        let own = inner.clone();
        let result = inner
            .handle()
            .unwrap()
            .block_on(async move {
                tokio::spawn(
                    async move { own.run(Duration::from_secs(1), "测试", async { Ok(()) }) },
                )
                .await
            })
            .unwrap();
        assert_eq!(result, Err(Error::AsyncContext));
        // 引擎运行时的阻塞线程（回调在这里跑）同样拒绝
        let own = inner.clone();
        let handle = inner.handle().unwrap();
        let result = handle
            .block_on(handle.spawn_blocking(move || {
                own.run(Duration::from_secs(1), "测试", async { Ok(()) })
            }))
            .unwrap();
        assert_eq!(result, Err(Error::AsyncContext));
    }

    #[test]
    fn 在别的运行时的阻塞线程和异步任务里也能等引擎() {
        // 像 Tauri 命令那样：调用方自己的 tokio 运行时，在它的阻塞线程池或异步任务里调用引擎的同步接口
        let engine = std::sync::Arc::new(Executor::new().unwrap());
        // 调用方的线程碰巧和引擎线程同名也不影响
        let host = Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("pierlink-ssh")
            .enable_all()
            .build()
            .unwrap();
        let blocking = engine.clone();
        let from_blocking = host
            .block_on(host.spawn_blocking(move || {
                blocking.run(Duration::from_secs(5), "测试", async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok(7)
                })
            }))
            .unwrap();
        assert_eq!(from_blocking, Ok(7));
        let task = engine.clone();
        let from_task = host
            .block_on(host.spawn(async move { task.wait(async { Ok("异步任务里") }) }))
            .unwrap();
        assert_eq!(from_task, Ok("异步任务里"));
        // 借用调用方的数据也可以（不要求 'static）
        let data = [1, 2, 3];
        let borrowed = engine.clone();
        let sum = host.block_on(async {
            tokio::task::block_in_place(|| {
                borrowed.run(Duration::from_secs(5), "测试", async {
                    Ok(data.iter().sum::<i32>())
                })
            })
        });
        assert_eq!(sum, Ok(6));
    }
}
