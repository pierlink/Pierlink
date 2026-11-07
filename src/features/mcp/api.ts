import { inTauri } from "../shell/TitleBar";
import type { AppError } from "../../errors";

/** 审批方式：ask_all 每次都问；dangerous 只问危险操作（默认）；auto 全自动、只记录 */
export type McpApproval = "ask_all" | "dangerous" | "auto";

export interface EntrySettings {
  enabled: boolean;
  /** 审批方式 */
  approval: McpApproval;
  /** 允许在 Windows 本机执行命令 */
  windowsCommands: boolean;
  /** 本机文件工具允许的文件夹（绝对路径） */
  folders: string[];
  /** 允许操作的服务器 ID */
  servers: string[];
  /** 新连接先请用户确认（只对服务器入口有效，默认不问） */
  confirmConnections: boolean;
}

/** 一台服务器上的入口 */
export interface ServerEntrySettings {
  serverId: string;
  /** 服务器上监听的端口（只在服务器的 127.0.0.1 上） */
  port: number;
  entry: EntrySettings;
}

/** AI 接入的全部设置 */
export interface McpSettings {
  enabled: boolean;
  windows: EntrySettings;
  servers: ServerEntrySettings[];
}

export interface McpSession {
  id: string;
  /** windows 或服务器 ID */
  entry: string;
  /** 客户端自报的名称和版本（不可信） */
  client: string;
  /** 有操作正在进行 */
  busy: boolean;
  /** 最后一次操作的时间（Unix 毫秒） */
  lastActiveMs: number;
}

export interface McpEntryState {
  serverId: string;
  /** connecting / running / reconnecting / port_busy（服务器上的端口被占着）/ waiting_password / error / stopped */
  state: string;
  port: number;
  error: AppError | null;
  /** 服务器上的 ~/.pierlink/mcp.json 已写好 */
  fileWritten: boolean;
  /** 端口被占着时查到是谁占着（还没查完时为 null） */
  conflict: McpPortConflict | null;
  /** 换令牌或端口后自动重新添加的结果（最近一次） */
  readded: McpReadd | null;
  /** 另一台电脑占着设置的端口，这台换了一个端口接入（从属入口）；主入口为 null */
  secondary?: McpSecondary | null;
}

/** 从属入口：经另一台电脑的主入口接入 */
export interface McpSecondary {
  /** 占着设置端口的那台电脑（知道的话） */
  via: string | null;
  /** 这台在服务器上实际监听的端口 */
  port: number;
}

/** 连着同一批服务器的一台电脑 */
export interface McpComputer {
  instance: string;
  name: string;
}

/** 几台电脑同时接入时的电脑列表和生效的指定 */
export interface McpComputers {
  /** 这台电脑的实例号 */
  thisInstance: string;
  /** 连着同一批服务器的电脑（含这台，这台在最前） */
  computers: McpComputer[];
  /** 用户指定 AI 操作的那台；没指定（自动判断）为 null */
  pinned: McpComputer | null;
}

/** 服务器上的端口被谁占着 */
export interface McpPortConflict {
  /** this_pc / other_pc / stale / other_program / not_listening / unknown（查不出来） */
  kind: string;
  /** 另一台电脑的名字（知道的话） */
  computer: string | null;
  /** 建议换到的空闲端口 */
  freePort: number | null;
}

/** 换令牌或端口后，在服务器上自动重新添加的结果 */
export interface McpReadd {
  /** token / port */
  reason: string;
  ok: string[];
  failed: string[];
  atMs: number;
}

/** 客户端配置里有没有加好这个入口：current 加好了 / stale 地址、令牌或程序位置是旧的 / none 没加 */
export type McpAdded = "current" | "stale" | "none";

/** 装了哪些客户端，以及各自加好没有（查不了的客户端不在 added 里） */
export interface McpClients {
  claude: boolean;
  codex: boolean;
  cursor: boolean;
  gemini: boolean;
  added: Partial<Record<string, McpAdded>>;
}

export interface McpStatus {
  /** Windows 入口在运行（agent 能连上来） */
  pipeRunning: boolean;
  pipeError: string | null;
  /** Pierlink 以管理员身份运行（AI 执行的命令也带管理员权限） */
  elevated: boolean;
  sessions: McpSession[];
  entries: McpEntryState[];
}

/** 一次工具调用（已脱敏） */
export interface McpActivity {
  timeMs: number;
  session: string;
  entry: string;
  client: string;
  tool: string;
  target: string | null;
  summary: string;
  /** ok / denied / cancelled / 错误码 */
  outcome: string;
  exitCode: number | null;
  durationMs: number;
  /** 谁批准的：user / session（这次连接不再问）/ auto（全自动）；空 = 本来就不用问 */
  approval: string;
  /** 命令输出的结尾（已脱敏，只在内存里）；没有输出的操作为 null */
  output?: { stdout: string; stderr: string } | null;
  /** 转给了哪台电脑执行（几台电脑同时接入时） */
  forwardedTo?: string | null;
  /** 哪台电脑转来的 */
  forwardedFrom?: string | null;
}

/** 请用户确认的内容 */
export interface McpConfirm {
  id: number;
  /** connect：新连接（入口设置了“每次连接都问”）；dangerous：危险命令或危险的文件写入；action：“每次都问”方式下的普通操作 */
  kind: "connect" | "dangerous" | "action";
  entry: string;
  client: string;
  targets: string[];
  target: string | null;
  action: string | null;
}

/** 请界面打开 AI 终端 */
export interface McpOpenTerminal {
  id: number;
  /** windows：本机 PowerShell；server：某台服务器 */
  kind: "windows" | "server";
  serverId: string | null;
  title: string | null;
}

export type McpNotice =
  | { kind: "loginNeeded"; serverId: string }
  | { kind: "passwordPrompt"; terminalId: string }
  | { kind: "terminalOpened"; terminalId: string }
  | { kind: "terminalClosed"; terminalId: string };

/** 给用户复制的接入命令 */
export interface McpConnectInfo {
  windows: { client: string; text: string }[];
  /** 每个服务器入口：在那台服务器上手动添加的命令，client 是 claude / gemini / codex */
  servers: { serverId: string; url: string; commands: { client: string; text: string }[] }[];
}

export type McpAnswer = "allow" | "allow_always" | "deny";

/** AI 开的一个后台任务 */
export interface McpJob {
  id: string;
  /** 哪个入口的 AI 开的：windows 或服务器 ID */
  entry: string;
  /** 在哪执行：windows 或服务器 ID */
  target: string;
  /** 命令（已遮掉密码、令牌） */
  command: string;
  /** 开始时间（Unix 毫秒） */
  startedMs: number;
  running: boolean;
  exitCode: number | null;
}

/** AI 接入接口：Tauri 实现和浏览器预览实现都遵守它 */
export interface McpApi {
  getSettings(): Promise<McpSettings>;
  /** 保存设置，返回规范化后的设置（文件夹去重等）；不合法时抛出错误 */
  setSettings(settings: McpSettings): Promise<McpSettings>;
  status(): Promise<McpStatus>;
  connectInfo(): Promise<McpConnectInfo>;
  rotateToken(serverId: string): Promise<void>;
  activity(limit: number): Promise<McpActivity[]>;
  stopAll(): Promise<void>;
  /** AI 开的后台任务（还在运行的和刚结束的） */
  jobs(): Promise<McpJob[]>;
  /** 停掉一个后台任务；返回 false 表示它已经不在了 */
  stopJob(id: string): Promise<boolean>;
  /** 回答确认框；返回 false 表示请求已经失效 */
  answer(id: number, answer: McpAnswer): Promise<boolean>;
  /** 还没回答的确认（页面重新加载后恢复） */
  pendingConfirms(): Promise<McpConfirm[]>;
  /** 回报 AI 终端打开的结果 */
  terminalOpened(id: number, terminalId: string | null, error: unknown): Promise<boolean>;
  openLogFolder(): Promise<void>;
  /** 让用户在 Windows 上选一个文件夹；取消时为 null */
  pickFolder(): Promise<string | null>;
  /** 用户接管终端（拒绝 AI 的输入）或交还给 AI */
  setUserControl(terminalId: string, takenOver: boolean): Promise<void>;
  /** 一键把本机入口加进 claude / codex / cursor；返回用的程序或改的文件 */
  installLocal(client: "claude" | "codex" | "cursor"): Promise<string>;
  /** 在服务器上把这台服务器的入口加进 Claude Code、Codex 或 Gemini CLI */
  installServer(serverId: string, client: "claude" | "codex" | "gemini"): Promise<void>;
  /** 把本机入口从 claude / codex / cursor 里移除；返回 false 表示本来就没有 */
  removeLocal(client: "claude" | "codex" | "cursor"): Promise<boolean>;
  /** 在服务器上把这台服务器的入口从客户端里移除；返回 false 表示本来就没有 */
  removeServer(serverId: string, client: "claude" | "codex" | "gemini"): Promise<boolean>;
  /** 这台电脑上装了哪些客户端、各自加好本机入口没有 */
  detectClients(): Promise<McpClients>;
  /** 服务器上装了哪些客户端、各自加好这台服务器的入口没有（要连过去查；连不上时抛错） */
  detectServerClients(serverId: string): Promise<McpClients>;
  /** 报告终端当前目录（AI 列终端时用） */
  reportCwd(terminalId: string, cwd: string | null): Promise<void>;
  onSessions(cb: () => void): Promise<() => void>;
  onActivity(cb: (a: McpActivity) => void): Promise<() => void>;
  onConfirm(cb: (c: McpConfirm) => void): Promise<() => void>;
  onConfirmClosed(cb: (id: number) => void): Promise<() => void>;
  /** 设置在后台被改了（确认框里点了“以后不再问”） */
  onSettings(cb: () => void): Promise<() => void>;
  onOpenTerminal(cb: (r: McpOpenTerminal) => void): Promise<() => void>;
  onNotice(cb: (n: McpNotice) => void): Promise<() => void>;
  onEntries(cb: (entries: McpEntryState[]) => void): Promise<() => void>;
  /** 后台任务有变化（开始、结束、被停掉） */
  onJobs(cb: () => void): Promise<() => void>;
  /** 连着同一批服务器的电脑和生效的指定（要到各服务器上读，稍慢） */
  computers(): Promise<McpComputers>;
  /** 指定 AI 操作哪台电脑；instance 为 null 是取消指定（自动判断） */
  setPin(instance: string | null, computer: string): Promise<void>;
  /** 这台电脑上改了指定 */
  onPin(cb: () => void): Promise<() => void>;
}

/** 默认设置（和 Rust 端一致）：全部关闭，本机入口默认允许执行命令 */
export function defaultSettings(): McpSettings {
  return {
    enabled: false,
    windows: { enabled: false, approval: "dangerous", windowsCommands: true, folders: [], servers: [], confirmConnections: false },
    servers: [],
  };
}

/** 服务器入口的默认设置：不给 Windows 命令、不给文件夹、不给其他服务器 */
export function defaultServerEntry(serverId: string): ServerEntrySettings {
  return { serverId, port: 47821, entry: { enabled: true, approval: "dangerous", windowsCommands: false, folders: [], servers: [], confirmConnections: false } };
}

/** 浏览器预览的示例数据（#mcp 打开 MCP 板块时用，方便看样稿） */
function sampleSettings(): McpSettings {
  return {
    enabled: true,
    windows: { enabled: true, approval: "dangerous", windowsCommands: true, folders: ["D:\\work\\pierlink", "D:\\work\\demo"], servers: ["s1", "s3"], confirmConnections: false },
    servers: [
      { serverId: "s1", port: 47821, entry: { enabled: true, approval: "dangerous", windowsCommands: true, folders: ["D:\\work\\web-app"], servers: [], confirmConnections: false } },
      { serverId: "s5", port: 47821, entry: { enabled: false, approval: "dangerous", windowsCommands: false, folders: [], servers: [], confirmConnections: true } },
    ],
  };
}
function sampleActivity(): McpActivity[] {
  const now = Date.now();
  const row = (min: number, entry: string, client: string, tool: string, target: string | null, summary: string, outcome = "ok", approval = ""): McpActivity =>
    ({ timeMs: now - min * 60000, session: "x", entry, client, tool, target, summary, outcome, exitCode: tool === "run_command" ? 0 : null, durationMs: 120, approval });
  return [
    row(1, "windows", "Claude Code 2.1", "run_command", "s1", "nvidia-smi"),
    row(2, "windows", "Claude Code 2.1", "terminal_send", "s1", "pane-3 python train.py --epochs 5 [Enter]"),
    row(4, "windows", "Claude Code 2.1", "run_command", "s1", "rm -rf /tmp/old-checkpoints", "ok", "user"),
    row(6, "windows", "Claude Code 2.1", "write_file", null, "D:\\work\\pierlink\\notes.md"),
    row(9, "s1", "Claude Code 2.1 (gpu-dev)", "run_command", "windows", "cargo build --release", "ok", "user"),
    row(12, "windows", "Cursor 1.4", "read_file", null, "D:\\work\\demo\\README.md"),
    row(15, "windows", "Claude Code 2.1", "run_command", "s3", "sudo reboot", "denied"),
    row(21, "windows", "Claude Code 2.1", "sync_wait", null, "pierlink-src"),
  ];
}

/** 浏览器预览：没有真正的后端，设置存在内存里；#settings-mcp-demo 演示连接确认；#mcp 带示例数据 */
function previewApi(): McpApi {
  const sample = window.location.hash.startsWith("#mcp");
  let settings = sample ? sampleSettings() : defaultSettings();
  const confirms = new Set<(c: McpConfirm) => void>();
  if (import.meta.env.DEV && window.location.hash.includes("mcp-demo")) {
    setTimeout(() => {
      for (const cb of confirms) cb({ id: 1, kind: "connect", entry: "windows", client: "Claude Code 2.1", targets: ["windows", "gpu-a100"], target: null, action: null });
    }, 500);
  }
  const noop = async () => () => {};
  // 预览：两台电脑，可以指定
  let pinned: McpComputer | null = null;
  const pins = new Set<() => void>();
  return {
    getSettings: async () => settings,
    setSettings: async (s) => { settings = s; return s; },
    status: async () => ({
      pipeRunning: true, pipeError: null, elevated: false,
      sessions: sample ? [
        { id: "a", entry: "windows", client: "Claude Code 2.1", busy: false, lastActiveMs: Date.now() - 180_000 },
        { id: "c", entry: "s1", client: "Claude Code 2.1", busy: true, lastActiveMs: Date.now() },
      ] : [],
      entries: sample ? [{ serverId: "s1", state: "running", port: 47821, error: null, fileWritten: true, conflict: null, readded: null, secondary: null }] : [],
    }),
    connectInfo: async () => ({
      windows: [
        { client: "claude", text: "claude mcp add --scope user pierlink -- \"C:\\Users\\you\\AppData\\Local\\Pierlink\\Pierlink.exe\" --mcp" },
        { client: "codex", text: "codex mcp add pierlink -- \"C:\\Users\\you\\AppData\\Local\\Pierlink\\Pierlink.exe\" --mcp" },
        { client: "json", text: '{\n  "mcpServers": {\n    "pierlink": {\n      "command": "C:\\\\Users\\\\you\\\\AppData\\\\Local\\\\Pierlink\\\\Pierlink.exe",\n      "args": ["--mcp"]\n    }\n  }\n}' },
      ],
      servers: settings.servers.map((s) => ({ serverId: s.serverId, url: `http://127.0.0.1:${s.port}/mcp`, commands: [
        { client: "claude", text: `claude mcp remove --scope user pierlink >/dev/null 2>&1; claude mcp add --transport http --scope user pierlink http://127.0.0.1:${s.port}/mcp --header "Authorization: Bearer $(jq -r .token ~/.pierlink/mcp.json)"` },
        { client: "gemini", text: `cd "$HOME" && gemini mcp add --scope user --transport http --header "Authorization: Bearer $(jq -r .token ~/.pierlink/mcp.json)" pierlink http://127.0.0.1:${s.port}/mcp` },
      ] })),
    }),
    rotateToken: async () => {},
    activity: async () => (sample ? sampleActivity() : []),
    stopAll: async () => {},
    jobs: async () => (sample ? [
      { id: "j1", entry: "s1", target: "windows", command: "npm run dev", startedMs: Date.now() - 600_000, running: true, exitCode: null },
      { id: "j2", entry: "windows", target: "windows", command: "ping -t 127.0.0.1", startedMs: Date.now() - 120_000, running: false, exitCode: 1 },
    ] : []),
    stopJob: async () => true,
    answer: async () => true,
    pendingConfirms: async () => [],
    terminalOpened: async () => true,
    openLogFolder: async () => {},
    pickFolder: async () => "D:\\work\\new-project",
    setUserControl: async () => {},
    reportCwd: async () => {},
    installLocal: async (client) => { await new Promise((r) => setTimeout(r, 600)); if (client === "codex") throw { code: "mcp.client_not_found", params: { client: "codex" } }; return client === "cursor" ? "C:\\Users\\you\\.cursor\\mcp.json" : "C:\\Users\\you\\.local\\bin\\claude.exe"; },
    installServer: async () => { await new Promise((r) => setTimeout(r, 900)); },
    removeLocal: async (client) => { await new Promise((r) => setTimeout(r, 500)); return client !== "cursor"; },
    removeServer: async () => { await new Promise((r) => setTimeout(r, 800)); return true; },
    detectClients: async () => ({ claude: true, codex: false, cursor: true, gemini: false, added: { claude: "current", cursor: "none" } }),
    detectServerClients: async () => { await new Promise((r) => setTimeout(r, 500)); return { claude: true, codex: true, cursor: false, gemini: true, added: { claude: "current", codex: "stale", gemini: "none" } }; },
    onSessions: noop,
    onActivity: noop,
    onConfirm: async (cb) => { confirms.add(cb); return () => { confirms.delete(cb); }; },
    onConfirmClosed: noop,
    onSettings: noop,
    onOpenTerminal: noop,
    onNotice: noop,
    onEntries: noop,
    onJobs: noop,
    computers: async () => ({
      thisInstance: "a",
      computers: sample ? [{ instance: "a", name: "OFFICE-PC" }, { instance: "b", name: "HOME-PC" }] : [{ instance: "a", name: "OFFICE-PC" }],
      pinned,
    }),
    setPin: async (instance, computer) => { pinned = instance ? { instance, name: computer } : null; for (const cb of pins) cb(); },
    onPin: async (cb) => { pins.add(cb); return () => { pins.delete(cb); }; },
  };
}

let api: Promise<McpApi> | null = null;

/** 取当前环境的 AI 接入接口（Tauri 里用真实后端，浏览器预览用示例） */
export function getMcpApi(): Promise<McpApi> {
  api ??= inTauri ? import("./tauriApi").then((m) => m.tauriMcpApi) : Promise.resolve(previewApi());
  return api;
}
