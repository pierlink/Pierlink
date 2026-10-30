import type { SyncTaskView } from "../features/sync/types";
import type { ProjectView, ResourceView, ServerView, TabView } from "../features/workspace/types";

/** 浏览器里调试界面用的示例数据（接入后端后只在组件预览中使用）。地址均为文档保留地址段。 */
export const mockServers: ServerView[] = [
  { id: "s1", name: "gpu-dev", address: "dev@203.0.113.18", group: null, favorite: true, status: "online", openTabs: 2, syncState: "warn" },
  { id: "s2", name: "hk-web-01", address: "deploy@198.51.100.7", group: null, favorite: true, status: "online", openTabs: 1, syncState: "ok" },
  { id: "s3", name: "build-arm", address: "ci@203.0.113.40", group: "lab", favorite: false, status: "connecting", openTabs: 1, syncState: "none" },
  { id: "s4", name: "lab-nas", address: "admin@192.0.2.12", group: "lab", favorite: false, status: "offline", openTabs: 0, syncState: "ok" },
  { id: "s5", name: "sg-api-01", address: "ops@198.51.100.21", group: "生产", favorite: false, status: "offline", openTabs: 0, syncState: "none" },
];

export const mockTabs: TabView[] = [
  { id: "t1", kind: "remote", title: "gpu-dev", subtitle: "~/projects/web-api", status: "online" },
  { id: "t2", kind: "remote", title: "gpu-dev", subtitle: "~/ml-train", status: "online" },
  { id: "t3", kind: "local", title: "PowerShell 7", subtitle: "D:\\code\\web-api", status: "online" },
  { id: "t4", kind: "remote", title: "build-arm", subtitle: "…", status: "connecting" },
];

export const mockResources: ResourceView[] = [
  { label: "cpu", value: "37%", percent: 37, detail: "16 核 · 负载 2.41", tone: "accent" },
  { label: "memory", value: "37%", percent: 37, detail: "23.1 / 62.6 GB", tone: "accent" },
  { label: "disk", value: "388 GB", percent: 61, detail: "61% · 1 TB", tone: "accent" },
  { label: "gpu", value: "91%", percent: 91, detail: "21.9 / 24 GB", tone: "warn" },
];

export const mockProjects: ProjectView[] = [
  { id: "p1", name: "web-api", remotePath: "~/projects/web-api", localPath: "D:\\code\\web-api", state: "已同步", tone: "ok", percent: 100, detail: "3 秒前 · 1,284 个文件", conflicts: 1 },
  { id: "p2", name: "ml-train", remotePath: "~/ml-train", localPath: "D:\\code\\ml-train", state: "同步中 62%", tone: "accent", percent: 62, detail: "剩余 14 个文件 · 38 MB", conflicts: 0 },
  { id: "p3", name: "datasets", remotePath: "~/datasets", localPath: null, state: "", tone: "ok", percent: 0, detail: "", conflicts: 0 },
];

const MB = 1024 * 1024;

/** 同步页示例数据 */
export const mockSyncTasks: SyncTaskView[] = [
  {
    id: "pl-web-api", localMode: "sendReceive", localChanges: 0, name: "web-api", localPath: "D:\\code\\web-api", state: "conflict", percent: 100,
    detail: "已同步 · 3 秒前 · 1,284 个文件",
    passwordServerId: null,
    loginServerId: null,
    eventsHasMore: false,
    endpoints: [
      { serverId: "s1", serverName: "gpu-dev", remotePath: "/home/dev/projects/web-api", mode: "sendReceive", link: "online" },
      { serverId: "s2", serverName: "hk-web-01", remotePath: "/srv/web-api", mode: "receiveOnly", link: "online" },
    ],
    files: 1284, bytes: 46 * MB, excludedFiles: 31906, excludedBytes: 412 * MB, versionsBytes: 12 * MB,
    ignoreRules: ["(?d)node_modules", "(?d).venv", "(?d)__pycache__", "(?d)dist", "(?d).cache", "*.log", ".env", "*.pem", "*.key", "(?d).git"],
    collaboration: false, watchDelayS: 10, conflictMode: "ask",
    conflicts: [{ path: "src/routes/order.ts", otherName: "order.sync-conflict-20261021-093052-K7Q2.ts", from: "gpu-dev" }],
    events: [
      { id: 1, by: null, time: "09:31:07", kind: "down", server: "gpu-dev", text: "src/routes/order.test.ts, src/routes/order.ts" },
      { id: 2, by: null, time: "09:30:52", kind: "conflict", server: "gpu-dev", text: "src/routes/order.ts" },
      { id: 3, by: null, time: "09:29:40", kind: "up", server: null, text: "README.md" },
      { id: 4, by: null, time: "09:12:03", kind: "link", server: "hk-web-01", text: "127.0.0.1:22101" },
    ],
  },
  {
    id: "pl-ml-train", localMode: "receiveOnly", localChanges: 0, name: "ml-train", localPath: "D:\\code\\ml-train", state: "syncing", percent: 62,
    detail: "剩余 14 个文件 · 38 MB · 约 40 秒",
    passwordServerId: null,
    loginServerId: null,
    eventsHasMore: false,
    endpoints: [{ serverId: "s1", serverName: "gpu-dev", remotePath: "/home/dev/ml-train", mode: "sendReceive", link: "online" }],
    files: 316, bytes: 84 * MB, excludedFiles: 1832, excludedBytes: 1100 * MB, versionsBytes: 3 * MB,
    ignoreRules: ["(?d).venv", "(?d)__pycache__", "checkpoints", "wandb", ".env"],
    collaboration: true, watchDelayS: 1, conflictMode: "newest", conflicts: [], events: [],
  },
  {
    id: "pl-firmware", localMode: "sendReceive", localChanges: 0, name: "firmware", localPath: "D:\\code\\firmware", state: "error", percent: 87,
    detail: "SSH 隧道断开，25 秒后自动重连；未同步的变更会在恢复后继续",
    passwordServerId: null,
    loginServerId: null,
    eventsHasMore: false,
    endpoints: [{ serverId: "s3", serverName: "build-arm", remotePath: "/home/ci/firmware", mode: "sendReceive", link: "reconnecting", retryInS: 25 }],
    files: 2210, bytes: 31 * MB, excludedFiles: 120, excludedBytes: 2 * MB, versionsBytes: 0,
    ignoreRules: ["(?d)build", "(?d).cache"], collaboration: false, watchDelayS: 10, conflictMode: "ask", conflicts: [], events: [],
  },
  {
    id: "pl-notes", localMode: "sendReceive", localChanges: 0, name: "notes", localPath: "D:\\notes", state: "paused", percent: 100,
    detail: "昨天 22:14 暂停",
    passwordServerId: null,
    loginServerId: null,
    eventsHasMore: false,
    endpoints: [{ serverId: "s4", serverName: "lab-nas", remotePath: "/data/notes", mode: "sendReceive", link: "offline" }],
    files: 98, bytes: 2 * MB, excludedFiles: 0, excludedBytes: 0, versionsBytes: null,
    ignoreRules: [], collaboration: false, watchDelayS: 10, conflictMode: "ask", conflicts: [], events: [],
  },
];
