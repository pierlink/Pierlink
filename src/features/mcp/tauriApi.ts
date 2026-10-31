import { listen } from "@tauri-apps/api/event";
import { commands } from "../../bindings/commands";
import { isAppError, type AppError } from "../../errors";
import type { McpActivity, McpApi, McpConfirm, McpConnectInfo, McpEntryState, McpNotice, McpOpenTerminal, McpSettings, McpStatus } from "./api";

/** 与 Rust 端 mcp/mod.rs 的事件名一致 */
const SESSIONS_EVENT = "pierlink://mcp-sessions";
const ACTIVITY_EVENT = "pierlink://mcp-activity";
const CONFIRM_EVENT = "pierlink://mcp-confirm";
const CONFIRM_CLOSED_EVENT = "pierlink://mcp-confirm-closed";
const OPEN_TERMINAL_EVENT = "pierlink://mcp-open-terminal";
const NOTICE_EVENT = "pierlink://mcp-notice";
const ENTRIES_EVENT = "pierlink://mcp-entries";
const JOBS_EVENT = "pierlink://mcp-jobs";

/** tauri-specta 对可能失败的命令返回 {status, data|error}；这里统一转成“成功返回数据、失败抛出错误” */
function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 打开失败的原因：界面拿到的错误不一定是后端错误，统一成 AppError 交回去 */
function toAppError(error: unknown): AppError | null {
  if (error === null || error === undefined) return null;
  if (isAppError(error)) return error;
  return { code: "mcp.terminal_failed", params: { reason: String(error) } };
}

export const tauriMcpApi: McpApi = {
  getSettings: async () => unwrap(await commands.mcpSettingsGet()) as McpSettings,
  setSettings: async (s) => unwrap(await commands.mcpSettingsSet(s)) as McpSettings,
  status: async () => unwrap(await commands.mcpStatus()) as McpStatus,
  connectInfo: async () => unwrap(await commands.mcpConnectInfo()) as McpConnectInfo,
  rotateToken: async (serverId) => { unwrap(await commands.mcpTokenRotate(serverId)); },
  activity: async (limit) => unwrap(await commands.mcpActivity(limit)) as McpActivity[],
  stopAll: async () => { unwrap(await commands.mcpStopAll()); },
  // 时间戳在绑定里是 number | null（f64 可能不是有限数）：实际总有值，缺了按 0 处理
  jobs: async () => unwrap(await commands.mcpJobs()).map((j) => ({ ...j, startedMs: j.startedMs ?? 0 })),
  stopJob: async (id) => unwrap(await commands.mcpJobStop(id)),
  answer: async (id, answer) => unwrap(await commands.mcpConfirmAnswer(id, answer)),
  pendingConfirms: async () => unwrap(await commands.mcpConfirmPending()) as McpConfirm[],
  terminalOpened: async (id, terminalId, error) => unwrap(await commands.mcpTerminalOpened(id, terminalId, terminalId ? null : toAppError(error))),
  openLogFolder: async () => { unwrap(await commands.mcpOpenLogFolder()); },
  pickFolder: async () => unwrap(await commands.pickFolder()),
  setUserControl: async (terminalId, takenOver) => { unwrap(await commands.terminalSetUserControl(terminalId, takenOver)); },
  handoverEntries: async (terminalId) => unwrap(await commands.mcpHandoverEntries(terminalId)),
  handOver: async (terminalId, entry) => { unwrap(await commands.mcpHandOver(terminalId, entry)); },
  takeBack: async (terminalId) => { unwrap(await commands.mcpTakeBack(terminalId)); },
  installLocal: async (client) => unwrap(await commands.mcpInstallLocal(client)).detail,
  installServer: async (serverId, client) => { unwrap(await commands.mcpInstallServer(serverId, client)); },
  removeLocal: async (client) => unwrap(await commands.mcpRemoveLocal(client)).removed,
  removeServer: async (serverId, client) => unwrap(await commands.mcpRemoveServer(serverId, client)).removed,
  detectClients: async () => unwrap(await commands.mcpDetectClients()),
  detectServerClients: async (serverId) => unwrap(await commands.mcpDetectServerClients(serverId)),
  reportCwd: async (terminalId, cwd) => { unwrap(await commands.terminalReportCwd(terminalId, cwd)); },
  onSessions: (cb) => listen(SESSIONS_EVENT, () => cb()),
  onJobs: (cb) => listen(JOBS_EVENT, () => cb()),
  onActivity: (cb) => listen<McpActivity>(ACTIVITY_EVENT, (e) => cb(e.payload)),
  onConfirm: (cb) => listen<McpConfirm>(CONFIRM_EVENT, (e) => cb(e.payload)),
  onConfirmClosed: (cb) => listen<number>(CONFIRM_CLOSED_EVENT, (e) => cb(e.payload)),
  onOpenTerminal: (cb) => listen<McpOpenTerminal>(OPEN_TERMINAL_EVENT, (e) => cb(e.payload)),
  onNotice: (cb) => listen<McpNotice>(NOTICE_EVENT, (e) => cb(e.payload)),
  onEntries: (cb) => listen<McpEntryState[]>(ENTRIES_EVENT, (e) => cb(e.payload)),
};
