//! 后台任务归入口所有；输出环形缓冲最多 1 MiB，完成记录保留 30 分钟。
use super::{check_cancel, local::OutputSink};
use crate::{
    error::AppError,
    mcp::{Entry, McpHost, RunOutput, Target, TargetKind, lock, text},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
const CAPACITY: usize = 1024 * 1024;
const RETENTION: Duration = Duration::from_secs(30 * 60);
#[derive(Default)]
pub(in crate::mcp) struct Jobs {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}
struct Job {
    id: String,
    entry: Entry,
    target: Target,
    cwd: Option<String>,
    shell: String,
    cancelled: Arc<AtomicBool>,
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    ring: VecDeque<(bool, Vec<u8>)>,
    bytes: usize,
    cursor: u64,
    finished: Option<Instant>,
    exit_code: Option<i32>,
    error: Option<AppError>,
    timed_out: bool,
    preview_out: VecDeque<u8>,
    preview_err: VecDeque<u8>,
}
impl State {
    fn push(&mut self, stderr: bool, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let preview = if stderr {
            &mut self.preview_err
        } else {
            &mut self.preview_out
        };
        preview.extend(bytes[bytes.len().saturating_sub(16384)..].iter().copied());
        preview.drain(..preview.len().saturating_sub(16384));
        self.cursor = self.cursor.saturating_add(bytes.len() as u64);
        if bytes.len() >= CAPACITY {
            self.ring.clear();
            self.bytes = CAPACITY;
            self.ring
                .push_back((stderr, bytes[bytes.len() - CAPACITY..].to_vec()));
            return;
        }
        self.bytes += bytes.len();
        self.ring.push_back((stderr, bytes.to_vec()));
        while self.bytes > CAPACITY {
            let excess = self.bytes - CAPACITY;
            if let Some((_, front)) = self.ring.front_mut() {
                if front.len() > excess {
                    front.drain(..excess);
                    self.bytes -= excess;
                } else {
                    self.bytes -= front.len();
                    self.ring.pop_front();
                }
            } else {
                break;
            }
        }
    }
    fn output(&self, since: u64) -> Value {
        let base = self.cursor.saturating_sub(self.bytes as u64);
        let mut position = base;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for (err, bytes) in &self.ring {
            let skip = since.saturating_sub(position).min(bytes.len() as u64) as usize;
            if *err {
                stderr.extend_from_slice(&bytes[skip..]);
            } else {
                stdout.extend_from_slice(&bytes[skip..]);
            }
            position += bytes.len() as u64;
        }
        json!({"stdout":text::strip_ansi(&stdout),"stderr":text::strip_ansi(&stderr),"running":self.finished.is_none(),"exit_code":self.exit_code,"cursor":self.cursor,"lost":since < base,"timed_out":self.timed_out,"error":self.error})
    }
}
pub(super) struct Spec {
    pub entry: Entry,
    pub target: Target,
    pub cwd: Option<String>,
    pub shell: String,
}
impl Jobs {
    pub(super) fn start(
        &self,
        spec: Spec,
        request: &AtomicBool,
        run: impl FnOnce(Arc<AtomicBool>, OutputSink) -> Result<RunOutput, AppError> + Send + 'static,
        completed: impl FnOnce(Value) + Send + 'static,
    ) -> Result<String, AppError> {
        let Spec {
            entry,
            target,
            cwd,
            shell,
        } = spec;
        let mut jobs = lock(&self.jobs);
        check_cancel(request)?;
        if jobs
            .values()
            .filter(|j| j.entry == entry && lock(&j.state).finished.is_none())
            .count()
            >= 8
        {
            return Err(AppError::new("mcp.job_limit").with("max", 8).with_reason(
                "mcp.this_entry_allows_at_most_8_concurrent_background_tasks",
                "这个入口最多同时运行 8 个后台任务",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let job = Arc::new(Job {
            id: id.clone(),
            entry,
            target,
            cwd,
            shell,
            cancelled: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(State::default()),
        });
        jobs.insert(id.clone(), job.clone());
        if std::thread::Builder::new()
            .name("MCP 后台任务".into())
            .spawn(move || {
                let sink_job = job.clone();
                let sink: OutputSink =
                    Arc::new(move |stderr, bytes| lock(&sink_job.state).push(stderr, bytes));
                let result = run(job.cancelled.clone(), sink);
                let value = {
                    let mut state = lock(&job.state);
                    match result {
                        Ok(output) => {
                            state.exit_code = output.exit_code;
                            state.timed_out = output.timed_out;
                        }
                        Err(mut error) => {
                            for value in error.params.values_mut() {
                                *value = crate::security::redact(value, &[]);
                            }
                            state.error = Some(error);
                        }
                    }
                    state.finished = Some(Instant::now());
                    let mut value = state.output(0);
                    value["job_id"] = json!(job.id);
                    value["target"] = json!(job.target.id);
                    value["stdout"] = json!(text::strip_ansi(
                        &state.preview_out.iter().copied().collect::<Vec<_>>()
                    ));
                    value["stderr"] = json!(text::strip_ansi(
                        &state.preview_err.iter().copied().collect::<Vec<_>>()
                    ));
                    value
                };
                completed(value);
            })
            .is_err()
        {
            jobs.remove(&id);
            return Err(AppError::new("mcp.process_failed").with_reason(
                "mcp.couldn_t_create_the_background_task_thread",
                "无法创建后台任务线程",
            ));
        }
        Ok(id)
    }
    fn get(&self, entry: &Entry, id: &str) -> Result<Arc<Job>, AppError> {
        lock(&self.jobs)
            .get(id)
            .filter(|j| &j.entry == entry)
            .cloned()
            .ok_or_else(|| {
                AppError::new("mcp.job_not_found").with_reason(
                    "mcp.background_task_is_missing_from_this_entry_or_its_record",
                    "这个入口没有该后台任务，或记录已过期",
                )
            })
    }
    pub(super) fn authorize(
        &self,
        entry: &Entry,
        id: &str,
        permissions: &crate::mcp::Permissions,
    ) -> Result<(), AppError> {
        let job = self.get(entry, id)?;
        let allowed = match job.target.kind {
            TargetKind::Windows => permissions.windows_commands,
            TargetKind::Server => permissions.servers.contains(&job.target.id),
        };
        if allowed {
            Ok(())
        } else {
            Err(AppError::new("mcp.target_denied").with_reason(
                "mcp.permission_for_the_background_task_target_was_revoked",
                "后台任务的目标权限已收回",
            ))
        }
    }
    pub(super) fn output(&self, entry: &Entry, id: &str, since: u64) -> Result<Value, AppError> {
        let job = self.get(entry, id)?;
        let state = lock(&job.state);
        if since > state.cursor {
            return Err(AppError::new("mcp.invalid_argument")
                .with_reason("mcp.output_cursor_is_out_of_range", "输出游标超出范围"));
        }
        let mut value = state.output(since);
        value["job_id"] = json!(id);
        value["cwd"] = json!(job.cwd);
        value["shell"] = json!(job.shell);
        Ok(value)
    }
    pub(super) fn stop(&self, entry: &Entry, id: &str) -> Result<Value, AppError> {
        let job = self.get(entry, id)?;
        job.cancelled.store(true, Ordering::Release);
        let started = Instant::now();
        while lock(&job.state).finished.is_none() {
            if started.elapsed() > Duration::from_secs(6) {
                return Err(AppError::new("mcp.job_stop_timeout").with_reason(
                    "mcp.stop_requested_process_cleanup_is_still_in_progress",
                    "任务已收到停止请求，仍在清理进程",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.output(entry, id, 0)
    }
    pub(super) fn list(&self, entry: &Entry) -> Value {
        let jobs = lock(&self.jobs);
        let mut values:Vec<_>=jobs.values().filter(|j| &j.entry==entry).map(|job| { let state=lock(&job.state); json!({"job_id":job.id,"target":job.target.id,"cwd":job.cwd,"shell":job.shell,"running":state.finished.is_none(),"exit_code":state.exit_code,"output_streaming":job.target.kind==TargetKind::Windows}) }).collect();
        values.sort_by(|a, b| a["job_id"].as_str().cmp(&b["job_id"].as_str()));
        json!(values)
    }
    pub(in crate::mcp) fn cancel_entry(&self, entry: Option<&Entry>) {
        for job in lock(&self.jobs)
            .values()
            .filter(|j| entry.is_none_or(|e| e == &j.entry))
        {
            job.cancelled.store(true, Ordering::Release);
        }
    }
    fn prune(&self) {
        lock(&self.jobs).retain(|_, job| {
            !lock(&job.state)
                .finished
                .is_some_and(|t| t.elapsed() >= RETENTION)
        });
    }
    pub(in crate::mcp) fn maintain(&self, host: &dyn McpHost) {
        self.prune();
        let jobs: Vec<_> = lock(&self.jobs).values().cloned().collect();
        for job in jobs {
            if lock(&job.state).finished.is_some() {
                continue;
            }
            let allowed = host
                .permissions(&job.entry)
                .ok()
                .flatten()
                .is_some_and(|p| match job.target.kind {
                    TargetKind::Windows => p.windows_commands,
                    TargetKind::Server => p.servers.contains(&job.target.id),
                });
            if !allowed {
                job.cancelled.store(true, Ordering::Release);
            }
        }
    }
}
impl Drop for Jobs {
    fn drop(&mut self) {
        self.cancel_entry(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> Spec {
        Spec {
            entry: Entry::Windows,
            target: Target {
                target: "windows".into(),
                id: "windows".into(),
                name: "Windows".into(),
                kind: TargetKind::Windows,
                online: None,
                engine: None,
                address: None,
            },
            cwd: None,
            shell: "sh".into(),
        }
    }
    #[test]
    fn 环形输出游标丢失标记和双流共用容量() {
        let mut state = State::default();
        state.push(false, b"hello");
        state.push(true, b"error");
        let first = state.output(3);
        assert_eq!(first["stdout"], "lo");
        assert_eq!(first["stderr"], "error");
        assert_eq!(first["cursor"], 10);
        assert_eq!(first["lost"], false);
        state.push(false, &vec![b'x'; CAPACITY + 20]);
        assert_eq!(state.bytes, CAPACITY);
        assert_eq!(state.output(0)["lost"], true);
        assert_eq!(state.output(state.cursor)["stdout"], "");
    }
    #[test]
    fn 任务入口隔离并发上限停止和清理() {
        let jobs = Jobs::default();
        let request = AtomicBool::new(false);
        let mut ids = Vec::new();
        for _ in 0..8 {
            ids.push(
                jobs.start(
                    spec(),
                    &request,
                    |stop, sink| {
                        sink(false, b"start");
                        while !stop.load(Ordering::Acquire) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Ok(RunOutput::default())
                    },
                    |_| {},
                )
                .unwrap(),
            );
        }
        assert_eq!(
            jobs.start(spec(), &request, |_, _| Ok(RunOutput::default()), |_| {})
                .unwrap_err()
                .code,
            "mcp.job_limit"
        );
        assert!(
            jobs.output(
                &Entry::Server {
                    server_id: "other".into()
                },
                &ids[0],
                0
            )
            .is_err()
        );
        assert_eq!(jobs.list(&Entry::Windows).as_array().unwrap().len(), 8);
        let stopped = jobs.stop(&Entry::Windows, &ids[0]).unwrap();
        assert_eq!(stopped["running"], false);
        jobs.cancel_entry(Some(&Entry::Windows));
        for id in ids {
            assert_eq!(jobs.stop(&Entry::Windows, &id).unwrap()["running"], false);
        }
    }
    #[cfg(unix)]
    #[test]
    fn 后台任务实时读取与停止孙进程() {
        let root = crate::test_support::tempdir();
        let pid = root.path().join("pid");
        let prepared = super::super::local::prepare(
            &format!(
                "printf ready; sleep 90 & echo $! > {}; wait",
                crate::ssh::quote::sh_quote(pid.to_str().unwrap()).unwrap()
            ),
            Some(root.path()),
            None,
        )
        .unwrap();
        let jobs = Jobs::default();
        let id = jobs
            .start(
                spec(),
                &AtomicBool::new(false),
                move |stop, sink| {
                    super::super::local::run_prepared(
                        &prepared,
                        crate::mcp::RunLimits {
                            timeout: Duration::from_secs(120),
                            output_bytes: 8192,
                        },
                        &stop,
                        Some(sink),
                    )
                },
                |_| {},
            )
            .unwrap();
        let started = Instant::now();
        loop {
            let output = jobs.output(&Entry::Windows, &id, 0).unwrap();
            if output["stdout"] == "ready" && pid.exists() {
                assert_eq!(output["running"], true);
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(jobs.output(&Entry::Windows, &id, 5).unwrap()["stdout"], "");
        let result = jobs.stop(&Entry::Windows, &id).unwrap();
        assert_eq!(result["running"], false);
        let pid = std::fs::read_to_string(pid).unwrap();
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
        assert!(stat.is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z"));
    }
    #[test]
    fn 完成记录保留三十分钟后整体释放() {
        let jobs = Jobs::default();
        let id = jobs
            .start(
                spec(),
                &AtomicBool::new(false),
                |_, _| Ok(RunOutput::default()),
                |_| {},
            )
            .unwrap();
        jobs.stop(&Entry::Windows, &id).unwrap();
        let job = jobs.get(&Entry::Windows, &id).unwrap();
        lock(&job.state).finished = Some(Instant::now() - RETENTION - Duration::from_secs(1));
        jobs.prune();
        assert!(jobs.get(&Entry::Windows, &id).is_err());
    }
}
