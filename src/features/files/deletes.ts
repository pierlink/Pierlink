import { useSyncExternalStore } from "react";
import { formatError } from "../../errors";
import { getFilesApi, type DeleteProgress } from "./api";
import { locKey, parentPath } from "./path";
import { notifyDirChanged } from "./transfers";
import type { Loc } from "./types";

/**
 * 一个删除任务（显示在文件面板下方的浮窗里，不挡别的操作）。
 * mode：normal 服务器上逐项删除，能看到正在删哪个、能停下；fast 服务器上用 rm -rf 一次删完；local 本机放进回收站。
 */
export interface DeleteJob {
  key: number;
  /** 在哪台机器上删（locKey） */
  loc: string;
  /** 选中了几项 */
  count: number;
  mode: "normal" | "fast" | "local";
  state: "running" | "failed";
  /** 已经删掉几项、正在删哪个（逐项删除时才有） */
  deleted: number;
  current: string | null;
  error: string | null;
  /** 后端的任务 ID（停下用） */
  id: string;
  stopping: boolean;
}

let jobs: DeleteJob[] = [];
let seq = 0;
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((l) => l());
const subscribe = (l: () => void) => { listeners.add(l); return () => listeners.delete(l); };
const update = (key: number, patch: Partial<DeleteJob>) => {
  jobs = jobs.map((j) => (j.key === key ? { ...j, ...patch } : j));
  emit();
};
const drop = (key: number) => {
  jobs = jobs.filter((j) => j.key !== key);
  emit();
};

/** 当前所有删除任务（不订阅变化；组件里用 useDeletes） */
export function deleteJobs(): DeleteJob[] {
  return jobs;
}

/** 当前所有删除任务 */
export function useDeletes(): DeleteJob[] {
  return useSyncExternalStore(subscribe, deleteJobs);
}

/**
 * 开始删除（确认框已经关掉，删除在后台进行）。删完、停下后浮窗里这一项消失；失败时留着写原因，点 × 关掉。
 * 不管结果如何，都通知所在目录刷新。
 * @param loc 在哪台机器上
 * @param paths 要删的完整路径
 * @param fast 服务器上用 rm -rf 一次删完
 */
export async function startDelete(loc: Loc, paths: string[], fast: boolean): Promise<void> {
  const key = ++seq;
  const mode = loc.kind === "local" ? "local" : fast ? "fast" : "normal";
  const api = await getFilesApi();
  // 进度很频繁，合并到每帧最多更新一次
  let latest: DeleteProgress | null = null;
  let frame = 0;
  const run = api.remove(loc, paths, fast, (p) => {
    latest = p;
    if (!frame) frame = requestAnimationFrame(() => { frame = 0; if (latest) update(key, { deleted: latest.deleted, current: latest.current }); });
  });
  jobs = [...jobs, { key, loc: locKey(loc), count: paths.length, mode, state: "running", deleted: 0, current: null, error: null, id: run.id, stopping: false }];
  emit();
  try {
    await run.done;
    drop(key);
  } catch (e) {
    if ((e as { code?: string } | null)?.code === "sftp.cancelled") drop(key);
    else update(key, { state: "failed", error: formatError(e), current: null, stopping: false });
  } finally {
    if (frame) cancelAnimationFrame(frame);
    for (const dir of new Set(paths.map((p) => parentPath(p) ?? p))) notifyDirChanged(locKey(loc), dir);
  }
}

/** 停下逐项删除（已经删掉的不恢复） */
export async function stopDelete(key: number): Promise<void> {
  const job = jobs.find((j) => j.key === key);
  if (!job || job.state !== "running" || job.mode !== "normal" || job.stopping) return;
  update(key, { stopping: true });
  await (await getFilesApi()).cancel(job.id);
}

/** 关掉失败的删除任务 */
export function dismissDelete(key: number): void {
  jobs = jobs.filter((j) => j.key !== key || j.state === "running");
  emit();
}
