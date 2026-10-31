import { useSyncExternalStore } from "react";
import { formatError } from "../../errors";
import { getFilesApi } from "./api";
import { locKey } from "./path";
import type { ConflictChoice, TransferProgressView, TransferReportView, TransferRequest } from "./types";

/** 一个传输任务（显示在右下角的传输列表里） */
export interface TransferJob {
  key: number;
  req: TransferRequest;
  /** 显示用：“3 项 → 服务器名 /home/dev” */
  label: string;
  state: "running" | "done" | "failed" | "cancelled";
  progress: TransferProgressView | null;
  report: TransferReportView | null;
  error: string | null;
  /** 后端的传输 ID（取消用） */
  id: string | null;
  cancelRequested: boolean;
  /** 结束时间（毫秒）；进行中为 null */
  finishedAt: number | null;
}

/** 已结束的任务最多保留多少条（本次运行内，不写盘） */
const KEEP_FINISHED = 50;

let jobs: TransferJob[] = [];
let seq = 0;
const listeners = new Set<() => void>();
const changed = new Set<(loc: string, dir: string) => void>();
const emit = () => listeners.forEach((l) => l());
const subscribe = (l: () => void) => { listeners.add(l); return () => listeners.delete(l); };
const update = (key: number, patch: Partial<TransferJob>) => {
  jobs = jobs.map((j) => (j.key === key ? { ...j, ...patch } : j));
  if (patch.finishedAt) {
    // 超出保留数量时丢掉最早结束的
    const ended = jobs.filter((j) => j.finishedAt).sort((a, b) => a.finishedAt! - b.finishedAt!);
    const drop = new Set(ended.slice(0, Math.max(0, ended.length - KEEP_FINISHED)).map((j) => j.key));
    if (drop.size) jobs = jobs.filter((j) => !drop.has(j.key));
  }
  emit();
};

/** 当前所有传输任务 */
export function useTransfers(): TransferJob[] {
  return useSyncExternalStore(subscribe, () => jobs);
}

/**
 * 订阅“某台机器某个目录的内容变了”（传输完成后通知，面板据此刷新）。
 * @returns 取消订阅
 */
export function onDirChanged(fn: (loc: string, dir: string) => void): () => void {
  changed.add(fn);
  return () => changed.delete(fn);
}

/** 开始一个传输任务 */
export async function startTransfer(req: TransferRequest, conflict: ConflictChoice, label: string): Promise<void> {
  const key = ++seq;
  jobs = [...jobs, { key, req, label, state: "running", progress: null, report: null, error: null, id: null, cancelRequested: false, finishedAt: null }];
  emit();
  const api = await getFilesApi();
  // 进度回调很频繁，合并到每帧最多更新一次
  let latest: TransferProgressView | null = null;
  let frame = 0;
  const run = api.transfer(req, conflict, (p) => {
    latest = p;
    if (!frame) frame = requestAnimationFrame(() => { frame = 0; update(key, { progress: latest }); });
  });
  void run.id.then((id) => {
    update(key, { id });
    if (jobs.find((j) => j.key === key)?.cancelRequested) void api.cancel(id);
  }, () => {});
  try {
    const report = await run.done;
    update(key, { state: "done", report, finishedAt: Date.now() });
  } catch (e) {
    const cancelled = (e as { code?: string } | null)?.code === "sftp.cancelled";
    update(key, { state: cancelled ? "cancelled" : "failed", error: cancelled ? null : formatError(e), finishedAt: Date.now() });
  } finally {
    if (frame) cancelAnimationFrame(frame);
    // 目标目录内容变了（失败时也可能已传了一部分）
    changed.forEach((fn) => fn(locKey(req.to), req.destDir));
  }
}

/** 取消正在进行的传输（后端 ID 还没拿到时，拿到后立即取消） */
export async function cancelTransfer(key: number): Promise<void> {
  const job = jobs.find((j) => j.key === key);
  if (!job || job.state !== "running") return;
  update(key, { cancelRequested: true });
  if (job.id) await (await getFilesApi()).cancel(job.id);
}

/** 从列表里移除已结束的任务 */
export function dismissTransfer(key: number): void {
  jobs = jobs.filter((j) => j.key !== key || j.state === "running");
  emit();
}

/** 清除所有已结束的任务 */
export function clearFinishedTransfers(): void {
  jobs = jobs.filter((j) => j.state === "running");
  emit();
}

/** 总进度：进行中的任务个数和百分比 */
export function runningSummary(list: TransferJob[]): { count: number; percent: number } {
  const running = list.filter((j) => j.state === "running");
  const total = running.reduce((a, j) => a + (j.progress?.totalBytes ?? 0), 0);
  const done = running.reduce((a, j) => a + (j.progress?.doneBytes ?? 0), 0);
  return { count: running.length, percent: total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 0 };
}

/** 结果里需要用户注意的项数（失败、改名、跳过） */
export function issuesOf(job: TransferJob): number {
  const r = job.report;
  return r ? r.failed.length + r.renamed.length + r.skipped.length : 0;
}

/** 是否失败（整个任务失败，或部分文件失败） */
export function jobFailed(job: TransferJob): boolean {
  return job.state === "failed" || (job.state === "done" && !!job.report?.failed.length);
}
