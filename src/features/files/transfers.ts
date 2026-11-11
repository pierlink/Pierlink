import { useSyncExternalStore } from "react";
import { formatError } from "../../errors";
import { getFilesApi } from "./api";
import { locKey } from "./path";
import type { ConflictChoice, DragOutLabels, DragOutResult, Loc, TransferProgressView, TransferReportView, TransferRequest } from "./types";

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

/** 当前所有传输任务（不订阅变化；组件里用 useTransfers） */
export function transferJobs(): TransferJob[] {
  return jobs;
}

/** 当前所有传输任务 */
export function useTransfers(): TransferJob[] {
  return useSyncExternalStore(subscribe, transferJobs);
}

/**
 * 订阅“某台机器某个目录的内容变了”（传输完成后通知，面板据此刷新）。
 * @returns 取消订阅
 */
export function onDirChanged(fn: (loc: string, dir: string) => void): () => void {
  changed.add(fn);
  return () => changed.delete(fn);
}

/** 通知某台机器某个目录的内容变了（删除完成后用；传输完成时这里自己通知） */
export function notifyDirChanged(loc: string, dir: string): void {
  changed.forEach((fn) => fn(loc, dir));
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

/**
 * 把服务器上的文件拖到窗口外（资源管理器、桌面等）：开始系统拖放，松手放到别的程序上时才下载。
 * 真正开始下载时（收到第一条进度）在传输列表里加一条，结束后更新；没开始下载就结束（没放下、拖回本窗口）时不加。
 * @param from 来源服务器
 * @param sources 要拖出的完整路径
 * @param label 传输列表里显示的说明
 * @param labels 系统进度框里的文字
 * @returns 拖放的结果；下载出错时抛出错误（取消是 sftp.cancelled）
 */
export async function startDragOut(from: Loc, sources: string[], label: string, labels: DragOutLabels): Promise<DragOutResult> {
  const api = await getFilesApi();
  // 目的地是别的程序，不知道具体目录
  const req: TransferRequest = { from, sources, to: { kind: "local" }, destDir: "" };
  let key: number | null = null;
  let finished = false;
  let latest: TransferProgressView | null = null;
  let frame = 0;
  /** 在传输列表里加上这一项，返回它的编号 */
  const add = (progress: TransferProgressView | null): number => {
    const k = ++seq;
    key = k;
    jobs = [...jobs, { key: k, req, label, state: "running", progress, report: null, error: null, id: run.id, cancelRequested: false, finishedAt: null }];
    emit();
    return k;
  };
  const run = api.dragOut(from, sources, labels, (p) => {
    // 进度和命令的结果谁先到不保证：结果之后才到的进度不要再加一条进行中的
    if (finished) return;
    if (key === null) { add(p.totalBytes > 0 ? p : null); return; }
    // 进度回调很频繁，合并到每帧最多更新一次
    const job = key;
    latest = p;
    if (!frame) frame = requestAnimationFrame(() => { frame = 0; update(job, { progress: latest }); });
  });
  try {
    const result = await run.done;
    finished = true;
    // 已经下载、给了对方，但开始下载的进度还没到：照样在列表里留一条
    const job = key ?? (result.kind === "dropped" ? add(null) : null);
    if (job !== null) update(job, result.kind === "dropped" ? { state: "done", report: result.report, finishedAt: Date.now() } : { state: "cancelled", finishedAt: Date.now() });
    return result;
  } catch (e) {
    finished = true;
    const cancelled = (e as { code?: string } | null)?.code === "sftp.cancelled";
    if (key !== null) update(key, { state: cancelled ? "cancelled" : "failed", error: cancelled ? null : formatError(e), finishedAt: Date.now() });
    throw e;
  } finally {
    if (frame) cancelAnimationFrame(frame);
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
