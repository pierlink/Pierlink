import { inTauri } from "../shell/TitleBar";
import { mockSyncTasks } from "../../dev/mock";
import type { SyncDraft } from "./NewSyncWizard";
import type { PathIssue, SyncEstimateView } from "./types";
import { upperDrive } from "./format";

/** 后端返回的任务（来自数据库） */
export interface SyncTaskRecord {
  id: string;
  folderId: string;
  name: string;
  localPath: string;
  endpoints: { serverId: string; serverName: string; remotePath: string; mode: "sendReceive" | "receiveOnly" }[];
  paused: boolean;
  collaboration: boolean;
  watchDelayS: number;
  conflictMode: "ask" | "newest";
  versioning: boolean;
  ignoreRules: string[];
  /** 本机一侧的方式 */
  localMode: "sendReceive" | "receiveOnly";
}

/** 编辑任务时可改的设置 */
export interface TaskSettingsInput {
  localMode: "sendReceive" | "receiveOnly";
  endpoints: { serverId: string; mode: "sendReceive" | "receiveOnly" }[];
  collaboration: boolean;
  watchDelayS: number;
  versioning: boolean;
  conflictMode: "ask" | "newest";
  rules: string[];
}

/** 任务的实时状态 */
export interface SyncStatusRecord {
  /** Syncthing 文件夹状态；stopped=本机同步服务未运行；busy=同步服务正忙，稍后再查 */
  state: string;
  files: number;
  bytes: number;
  needBytes: number;
  errors: { path: string; reason: string }[];
  /** needsPassword：没连上是因为缺密码（这次运行还没输），界面给出“输入密码”；needsLogin：要验证码、后台登不上，界面给出“登录” */
  servers: { serverId: string; connected: boolean; needsPassword?: boolean; needsLogin?: boolean; completion: number | null; error: unknown }[];
  /** 因一次删除大量文件被自动暂停 */
  deletionGuardPaused?: boolean;
  /** 本机只接收时，本机改过、不会上传的项数 */
  localChanges?: number;
}

export interface SyncEventRecord {
  /** 数据库里的编号：去重，和 timeMs 一起作为“加载更多”的位置 */
  id: number;
  timeMs: number;
  kind: "downloaded" | "uploaded" | "deleted" | "conflict" | "error" | "deviceConnected" | "deviceDisconnected" | "paused" | "stateChanged";
  server: string | null;
  path: string | null;
  detail: string | null;
  /** 改动是谁做的：local = 这台电脑，否则是服务器名称；不知道时为 null */
  by: string | null;
}

export interface ConflictRecord {
  path: string;
  conflictFile: string;
  fromDevice: string | null;
  conflictMtimeMs: number;
  currentMtimeMs: number;
}

export interface VersionRecord {
  path: string;
  versionTimeMs: number;
  modifiedMs: number;
  size: number;
}

/** 一端的回收站占用；serverId 为 null 表示本机，查不到时 files/bytes 为 null、error 为原因 */
export interface TrashEndpoint {
  serverId: string | null;
  name: string;
  files: number | null;
  bytes: number | null;
  error: unknown;
}

/** 冲突对比里一边的文件 */
export interface DiffFile { size: number; modifiedMs: number; encoding: string | null }
/** 并排对比的一行：left 是当前版本，right 是冲突副本 */
export interface DiffLine { leftNumber: number | null; rightNumber: number | null; left: string | null; right: string | null; kind: "equal" | "deleted" | "added" | "modified" }
export type ConflictDiff =
  | { kind: "text"; left: DiffFile; right: DiffFile; lines: DiffLine[] }
  | { kind: "binary"; left: DiffFile; right: DiffFile }
  | { kind: "tooLarge"; left: DiffFile; right: DiffFile; limit: number };

/** 关掉 Syncthing 管理页后的核对结果 */
export interface ConfigReport {
  /** 已静默恢复的加固项 */
  hardened: string[];
  changes: { index: number; target: string; id: string; taskName: string | null; field: string; expected: string; actual: string; adoptable: boolean }[];
  foreign: { target: string; id: string; name: string; path: string | null }[];
  /** 刚恢复的设置让这个任务暂停了：任务名 */
  pausedTask: string | null;
}

export interface SyncApi {
  deployedServers(): Promise<string[]>;
  tasks(): Promise<SyncTaskRecord[]>;
  status(taskId: string): Promise<SyncStatusRecord>;
  checkPaths(d: SyncDraft): Promise<PathIssue[]>;
  /** 开始前预估；onProgress 报告两端已扫描的文件数 */
  estimate(d: SyncDraft, onProgress?: (local: number, remote: number) => void): Promise<SyncEstimateView>;
  cancelEstimate(): Promise<void>;
  /** 一页事件（新的在前）；before 为上一页最后一条的位置，不传取最新的一页 */
  events(taskId: string, before?: { timeMs: number; id: number }): Promise<SyncEventRecord[]>;
  conflicts(taskId: string): Promise<ConflictRecord[]>;
  resolveConflict(taskId: string, c: ConflictRecord, keep: "current" | "other"): Promise<void>;
  versions(taskId: string, prefix?: string): Promise<VersionRecord[]>;
  restoreVersions(taskId: string, picks: { path: string; versionTimeMs: number }[]): Promise<{ path: string; reason: string }[]>;
  /** 各端回收站（旧版本目录）占用；本机在最前，连不上的端带错误 */
  trash(taskId: string): Promise<TrashEndpoint[]>;
  /** 给已有任务再加一台服务器（本机目录和规则沿用任务的）；start 为 true 时加好后开始同步 */
  addEndpoint(taskId: string, serverId: string, remotePath: string, mode: "sendReceive" | "receiveOnly", start: boolean): Promise<void>;
  /** 从任务里移除一台服务器（两边文件都保留；至少留一台） */
  removeEndpoint(taskId: string, serverId: string): Promise<void>;
  /** 这台电脑上的编辑器和这个任务默认用哪个 */
  editors(taskId: string): Promise<{ editors: { id: string; name: string }[]; preferred: string | null }>;
  /** 用编辑器打开任务的本机目录；editorId 为 null 用默认的（指定时会记住） */
  openInEditor(taskId: string, editorId: string | null): Promise<void>;
  /** 对比冲突的两个版本 */
  conflictDiff(taskId: string, path: string, conflictFile: string): Promise<ConflictDiff>;
  /** 在应用内窗口打开本机 Syncthing 管理页（自动登录） */
  openSyncthingUi(): Promise<void>;
  /** 还没处理完的核对结果 */
  configReport(): Promise<ConfigReport | null>;
  /** 处理一项被改过的设置：采用新值或恢复；返回剩下的 */
  configResolve(index: number, adopt: boolean): Promise<ConfigReport | null>;
  configDismiss(): Promise<void>;
  /** 关掉管理页后有要处理的核对结果时通知 */
  onConfigReport(cb: (r: ConfigReport) => void): Promise<() => void>;
  /** 立即清空一端的回收站；serverId 为 null 表示本机。返回删掉的量 */
  clearTrash(taskId: string, serverId: string | null): Promise<{ files: number; bytes: number }>;
  create(d: SyncDraft, start: boolean, watchDelayS: number, versioning: boolean): Promise<string>;
  setPaused(taskId: string, paused: boolean): Promise<void>;
  rescan(taskId: string): Promise<void>;
  remove(taskId: string): Promise<void>;
  /** 修改任务设置（两端的 Syncthing 配置和数据库一起改；任一端失败时恢复原样） */
  updateSettings(taskId: string, settings: TaskSettingsInput): Promise<void>;
  /** 本机只接收时：把本机改过的文件还原成服务器上的版本 */
  revertLocal(taskId: string): Promise<void>;
}

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

function draftInput(d: SyncDraft, watchDelayS: number, versioning: boolean) {
  return {
    serverId: d.serverId, remotePath: d.remotePath, localPath: d.localPath, mode: d.mode, localMode: d.localMode ?? "sendReceive", name: d.name,
    rules: d.rules, collaboration: d.collaboration, conflictMode: d.conflictMode, watchDelayS, versioning,
    ignoresSource: d.ignoresSource ?? null,
  };
}

/** 毫秒时间 → “月-日 时:分” */
function shortTime(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 后端预估结果 → 向导用的视图 */
function toEstimateView(e: {
  upload: { files: number; bytes: number }; download: { files: number; bytes: number }; same: { files: number; bytes: number }; conflict: { files: number; bytes: number };
  localTotal: { files: number; bytes: number }; remoteTotal: { files: number; bytes: number }; localSync: { files: number; bytes: number }; remoteSync: { files: number; bytes: number };
  conflicts: { path: string; localMtimeMs: number; remoteMtimeMs: number; newer: string }[];
  syncing: { path: string; files: number; bytes: number; reason: string | null }[];
  excluded: { path: string; files: number; bytes: number; reason: string | null }[];
  scannedLocal: number; scannedRemote: number;
}): SyncEstimateView {
  return {
    done: true, scannedLocal: e.scannedLocal, scannedRemote: e.scannedRemote,
    upload: e.upload, download: e.download, same: e.same.files,
    conflicts: e.conflicts.map((c) => ({ path: c.path, localTime: shortTime(c.localMtimeMs), remoteTime: shortTime(c.remoteMtimeMs), newer: c.newer === "local" ? "local" : "remote" })),
    conflictCount: e.conflict.files,
    remoteTotal: e.remoteTotal, localTotal: e.localTotal, remoteIncluded: e.remoteSync, localIncluded: e.localSync,
    included: e.syncing.map((d) => ({ path: d.path, files: d.files, bytes: d.bytes })),
    excluded: e.excluded.map((d) => ({ path: d.path, reason: (["dependency", "cache", "sensitive", "git"].includes(d.reason ?? "") ? d.reason : "custom") as SyncEstimateView["excluded"][number]["reason"], bytes: d.bytes })),
  };
}

async function createTauriSyncApi(): Promise<SyncApi> {
  const { commands } = await import("../../bindings/commands");
  const { Channel } = await import("@tauri-apps/api/core");
  return {
    async deployedServers() { return unwrap(await commands.syncDeployedServers()); },
    // 以前保存的任务可能是小写盘符：显示时统一大写
    async tasks() { return (unwrap(await commands.syncTasks()) as SyncTaskRecord[]).map((t) => ({ ...t, localPath: upperDrive(t.localPath) })); },
    async status(id) { return unwrap(await commands.syncTaskStatus(id)) as SyncStatusRecord; },
    async checkPaths(d) { return unwrap(await commands.syncCheckPaths(draftInput(d, 10, true))) as PathIssue[]; },
    async estimate(d, onProgress) {
      const ch = new Channel<{ scannedLocal: number; scannedRemote: number }>();
      ch.onmessage = (p) => onProgress?.(p.scannedLocal, p.scannedRemote);
      return toEstimateView(unwrap(await commands.syncEstimate(draftInput(d, 10, true), ch as never)) as never);
    },
    async cancelEstimate() { await commands.syncEstimateCancel(); },
    async events(id, before) { return unwrap(await commands.syncEvents(id, before ?? null)) as SyncEventRecord[]; },
    async conflicts(id) { return unwrap(await commands.syncConflicts(id)) as ConflictRecord[]; },
    async resolveConflict(id, c, keep) { unwrap(await commands.syncResolveConflict(id, c, keep)); },
    async trash(id) { return unwrap(await commands.syncTrash(id)) as TrashEndpoint[]; },
    async addEndpoint(id, serverId, remotePath, mode, start) { unwrap(await commands.syncAddEndpoint(id, serverId, remotePath, mode, start)); },
    async removeEndpoint(id, serverId) { unwrap(await commands.syncRemoveEndpoint(id, serverId)); },
    async editors(id) { return unwrap(await commands.syncEditors(id)); },
    async openInEditor(id, editorId) { unwrap(await commands.syncOpenInEditor(id, editorId)); },
    async conflictDiff(id, path, conflictFile) { return unwrap(await commands.syncConflictDiff(id, path, conflictFile)) as ConflictDiff; },
    async openSyncthingUi() { unwrap(await commands.syncOpenSyncthingUi()); },
    async configReport() { return unwrap(await commands.syncConfigReport()); },
    async configResolve(index, adopt) { return unwrap(await commands.syncConfigResolve(index, adopt)); },
    async configDismiss() { unwrap(await commands.syncConfigDismiss()); },
    async onConfigReport(cb) {
      const { listen } = await import("@tauri-apps/api/event");
      return listen<ConfigReport>("pierlink://sync-config-report", (e) => cb(e.payload));
    },
    async clearTrash(id, serverId) { const r = unwrap(await commands.syncClearTrash(id, serverId)); return { files: r.files ?? 0, bytes: r.bytes ?? 0 }; },
    async versions(id, prefix) { return unwrap(await commands.syncVersions(id, prefix ?? null)) as VersionRecord[]; },
    async restoreVersions(id, picks) { return unwrap(await commands.syncRestoreVersions(id, picks)); },
    async create(d, start, watchDelayS, versioning) { return unwrap(await commands.syncCreateTask(draftInput(d, watchDelayS, versioning), start)); },
    async setPaused(id, paused) { unwrap(await commands.syncSetPaused(id, paused)); },
    async rescan(id) { unwrap(await commands.syncRescan(id)); },
    async remove(id) { unwrap(await commands.syncRemoveTask(id)); },
    async updateSettings(id, settings) { unwrap(await commands.syncUpdateSettings(id, settings)); },
    async revertLocal(id) { unwrap(await commands.syncRevertLocal(id)); },
  };
}

/** 浏览器预览：示例任务 */
function createPreviewSyncApi(): SyncApi {
  let tasks: SyncTaskRecord[] = mockSyncTasks.map((t) => ({
    id: t.id, folderId: t.id, name: t.name, localPath: t.localPath, endpoints: t.endpoints.map((e) => ({ serverId: e.serverId, serverName: e.serverName, remotePath: e.remotePath, mode: e.mode })),
    paused: t.state === "paused", collaboration: t.collaboration, watchDelayS: t.watchDelayS, conflictMode: t.conflictMode, versioning: t.versionsBytes !== null, ignoreRules: t.ignoreRules,
    localMode: t.localMode,
  }));
  /** 预览：本机只接收的任务假装本机有 3 处改动，还原后清零 */
  const reverted = new Set<string>();
  return {
    async deployedServers() { return ["s1"]; },
    async tasks() { return tasks.map((t) => ({ ...t })); },
    async status(id) {
      const m = mockSyncTasks.find((x) => x.id === id);
      const t = tasks.find((x) => x.id === id);
      return {
        state: t?.paused ? "paused" : m?.state === "syncing" ? "syncing" : m?.state === "error" ? "error" : "idle",
        files: m?.files ?? 0, bytes: m?.bytes ?? 0, needBytes: m?.state === "syncing" ? 38 * 1024 * 1024 : 0, errors: [],
        servers: (t?.endpoints ?? []).map((e) => ({ serverId: e.serverId, connected: m?.state !== "error", completion: m?.state === "syncing" ? 62 : 100, error: null })),
        localChanges: t?.localMode === "receiveOnly" && !reverted.has(id) ? 3 : 0,
      };
    },
    async checkPaths() { return []; },
    async estimate(_d, onProgress) {
      for (let i = 1; i <= 5; i++) { onProgress?.(i * 60, i * 430); await new Promise((r) => setTimeout(r, 200)); }
      throw { code: "sync.estimate_unavailable", params: {} };
    },
    async cancelEstimate() {},
    async events(id, before) {
      if (before) return [];
      const m = mockSyncTasks.find((x) => x.id === id);
      const now = Date.now();
      return (m?.events ?? []).map((e, i) => ({ id: 1000 - i, timeMs: now - i * 60000, kind: ({ down: "downloaded", up: "uploaded", conflict: "conflict", link: "deviceConnected", error: "error", deleted: "deleted" } as const)[e.kind], server: e.server, path: e.text, detail: null, by: e.kind === "up" || e.kind === "deleted" ? "local" : e.kind === "down" ? (e.server ?? "gpu-dev") : null }));
    },
    async conflicts(id) {
      const m = mockSyncTasks.find((x) => x.id === id);
      return (m?.conflicts ?? []).map((c) => ({ path: c.path, conflictFile: c.otherName, fromDevice: c.from, conflictMtimeMs: Date.now() - 3600000, currentMtimeMs: Date.now() - 7200000 }));
    },
    async resolveConflict() {},
    async versions() { return [{ path: "src/routes/order.ts", versionTimeMs: Date.now() - 86400000, modifiedMs: Date.now() - 90000000, size: 4096 }, { path: "README.md", versionTimeMs: Date.now() - 3600000, modifiedMs: Date.now() - 4000000, size: 1200 }]; },
    async restoreVersions() { return []; },
    async addEndpoint() { await new Promise((r) => setTimeout(r, 600)); },
    async removeEndpoint() {},
    async editors() { return { editors: [{ id: "vscode", name: "VS Code" }, { id: "cursor", name: "Cursor" }], preferred: "vscode" }; },
    async openInEditor() {},
    async conflictDiff() {
      const f = (ms: number, size: number) => ({ size, modifiedMs: Date.now() - ms, encoding: "UTF-8" });
      return { kind: "text", left: f(7200000, 2310), right: f(3600000, 2388), lines: [
        { leftNumber: 1, rightNumber: 1, left: "import { Router } from \"express\";", right: "import { Router } from \"express\";", kind: "equal" },
        { leftNumber: 2, rightNumber: 2, left: "const router = Router();", right: "const router = Router();", kind: "equal" },
        { leftNumber: 3, rightNumber: 3, left: "router.get(\"/orders\", list);", right: "router.get(\"/orders\", listOrders);", kind: "modified" },
        { leftNumber: null, rightNumber: 4, left: null, right: "router.post(\"/orders\", createOrder);", kind: "added" },
        { leftNumber: 4, rightNumber: null, left: "// TODO: create", right: null, kind: "deleted" },
        { leftNumber: 5, rightNumber: 5, left: "export default router;", right: "export default router;", kind: "equal" },
      ] };
    },
    async openSyncthingUi() {},
    async configReport() { return null; },
    async configResolve() { return null; },
    async configDismiss() {},
    async onConfigReport() { return () => {}; },
    async trash() {
      return [
        { serverId: null, name: "", files: 342, bytes: 1288490188, error: null },
        { serverId: "gpu-dev", name: "gpu-dev", files: 57, bytes: 83886080, error: null },
      ];
    },
    async clearTrash(_id, serverId) { return serverId === null ? { files: 342, bytes: 1288490188 } : { files: 57, bytes: 83886080 }; },
    async create(d, start) {
      const id = `t${Date.now()}`;
      tasks = [...tasks, { id, folderId: id, name: d.name, localPath: d.localPath, endpoints: [{ serverId: d.serverId, serverName: d.serverId, remotePath: d.remotePath, mode: d.mode }], paused: !start, collaboration: d.collaboration, watchDelayS: 10, conflictMode: d.conflictMode, versioning: true, ignoreRules: d.rules, localMode: d.localMode ?? "sendReceive" }];
      return id;
    },
    async setPaused(id, paused) { tasks = tasks.map((t) => (t.id === id ? { ...t, paused } : t)); },
    async rescan() {},
    async remove(id) { tasks = tasks.filter((t) => t.id !== id); },
    async updateSettings(id, st) {
      await new Promise((r) => setTimeout(r, 400));
      tasks = tasks.map((t) => (t.id === id ? {
        ...t, localMode: st.localMode, collaboration: st.collaboration, watchDelayS: st.watchDelayS, versioning: st.versioning, conflictMode: st.conflictMode, ignoreRules: st.rules,
        endpoints: t.endpoints.map((e) => ({ ...e, mode: st.endpoints.find((x) => x.serverId === e.serverId)?.mode ?? e.mode })),
      } : t));
      reverted.delete(id);
    },
    async revertLocal(id) { await new Promise((r) => setTimeout(r, 300)); reverted.add(id); },
  };
}

let api: Promise<SyncApi> | null = null;
export function getSyncApi(): Promise<SyncApi> {
  api ??= inTauri ? createTauriSyncApi() : Promise.resolve(createPreviewSyncApi());
  return api;
}
