/**
 * 手机页面和 Windows 上手机入口之间的接口。同源请求、设备凭据在 HttpOnly Cookie 里；
 * POST 都带 X-Pierlink 头（入口据此拒绝跨站请求）。地址里带 ?mock 时用示例数据（开发预览）。
 */

export interface Me { name: string; permissions: { view: boolean; input: boolean; confirm: boolean } }
export interface OverviewServer { id: string; name: string; online: boolean | null; cpu: number | null; mem: number | null }
export interface OverviewSync { id: string; name: string; state: string; percent: number | null }
export interface Overview { servers: OverviewServer[]; sync: OverviewSync[]; ai: { sessions: number; pending: number } }
/** 一个打开着的终端；cwd、busy、last 是电脑端能给时才有（当前目录、是否有程序在跑、最后一行） */
export interface TerminalItem { id: string; title: string; kind: "windows" | "server"; server: string | null; busy: boolean | null; ai: boolean; cwd?: string | null; last?: string | null }
/** 屏幕：text 是纯文字；lines 是同样的行带颜色码（电脑端支持时才有）；password 表示在等密码 */
export interface Screen { text: string; lines?: string[]; password: boolean }
/** 往终端里输入：text 和 keys 二选一；paste 表示 text 按粘贴写入（多行不会一行一行地提交） */
export interface InputRequest { text?: string; keys?: string[]; paste?: boolean; confirmed?: boolean }
export interface ConfirmItem { id: number; kind: string; entry: string; client: string; targets?: string[]; target: string | null; action: string | null; ageMs: number }
export interface ActivityItem { timeMs: number; entry: string; client: string; tool: string; target: string | null; summary: string; outcome: string }

/** 接口返回的错误：status 是 HTTP 状态，body 是返回的 JSON（可能为空） */
export class ApiError extends Error {
  constructor(public status: number, public body: Record<string, unknown> | null) {
    super(typeof body?.message === "string" ? body.message : typeof body?.code === "string" ? body.code : `HTTP ${status}`);
  }
  /** 入口给的错误码（mobile.xxx） */
  get code(): string | null { return typeof this.body?.code === "string" ? this.body.code : null; }
}

const mock = new URLSearchParams(location.search).has("mock");

/** 一个请求最多等多久（毫秒）：手机切网络时请求可能挂好几分钟，排在后面的输入会一直等着 */
const REQUEST_TIMEOUT_MS = 20_000;

/** 带超时的 fetch：超时按网络错误处理（不是 ApiError） */
async function fetchWithin(path: string, init: RequestInit, timeoutMs: number): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    return await fetch(path, { ...init, signal: controller.signal });
  } finally {
    clearTimeout(timer);
  }
}

async function request<T>(method: "GET" | "POST", path: string, body?: unknown, timeoutMs = REQUEST_TIMEOUT_MS): Promise<T> {
  const res = await fetchWithin(path, {
    method,
    credentials: "same-origin",
    headers: method === "POST" ? { "Content-Type": "application/json", "X-Pierlink": "1" } : undefined,
    body: body === undefined ? undefined : JSON.stringify(body),
  }, timeoutMs);
  const text = await res.text();
  const json = text ? (JSON.parse(text) as unknown) : null;
  if (!res.ok) throw new ApiError(res.status, (json as Record<string, unknown>) ?? null);
  return json as T;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
let mockPaired = false;
let mockConfirms: ConfirmItem[] = [
  { id: 1, kind: "dangerous", entry: "gpu-dev", client: "Claude Code 2.1", target: "gpu-dev", action: "rm -rf build/", ageMs: 12000 },
];

/** 预览用的屏幕：像 Claude Code 在问要不要执行一条命令 */
function mockScreen(id: string): Screen {
  const lines = [
    "\x1b[38;5;174m✻\x1b[0m \x1b[1mWelcome to Claude Code!\x1b[0m",
    "",
    "> 把构建失败的原因找出来并修好",
    "",
    "\x1b[38;5;174m⏺\x1b[0m 我先看一下构建输出。",
    "",
    "\x1b[38;5;174m⏺\x1b[0m \x1b[1mBash\x1b[0m(npm run build)",
    "  ⎿  src/routes/order.ts:12:5 - \x1b[31merror\x1b[0m TS2322: Type 'string' is not assignable to type 'number'.",
    "",
    "╭─────────────────────────────────────────────────────────────╮",
    "│ Bash command                                                │",
    "│                                                             │",
    "│   rm -rf build/ && npm run build                            │",
    "│                                                             │",
    "│ Do you want to proceed?                                     │",
    "│ \x1b[36m❯ 1. Yes\x1b[0m                                                    │",
    "│   2. Yes, and don't ask again for rm commands               │",
    "│   3. No, and tell Claude what to do differently (\x1b[1mesc\x1b[0m)       │",
    "╰─────────────────────────────────────────────────────────────╯",
  ];
  return { text: lines.map((l) => l.replace(/\x1b\[[0-9;]*m/g, "")).join("\n"), lines: id === "a" ? lines : undefined, password: false };
}

export const api = {
  me: async (): Promise<Me> => {
    if (mock) { if (!mockPaired && !location.search.includes("paired")) throw new ApiError(401, null); return { name: "iPhone", permissions: { view: true, input: true, confirm: true } }; }
    return request("GET", "/api/me");
  },
  pair: async (code: string, deviceName: string): Promise<void> => {
    if (mock) { await sleep(1500); if (code.length !== 8) throw new ApiError(401, { code: "mobile.invalid_invite" }); mockPaired = true; return; }
    await request("POST", "/api/pair", { code, device_name: deviceName }, 130_000);
  },
  logout: async (): Promise<void> => { if (!mock) await request("POST", "/api/logout", {}); },
  overview: async (): Promise<Overview> => {
    if (mock) return { servers: [{ id: "s1", name: "gpu-dev", online: true, cpu: 34, mem: 61 }, { id: "s2", name: "hk-web-01", online: false, cpu: null, mem: null }], sync: [{ id: "t1", name: "web-api", state: "syncing", percent: 62 }, { id: "t2", name: "notes", state: "paused", percent: null }], ai: { sessions: 2, pending: mockConfirms.length } };
    return request("GET", "/api/overview");
  },
  terminals: async (): Promise<TerminalItem[]> => {
    if (mock) return [{ id: "a", title: "gpu-dev", kind: "server", server: "gpu-dev", busy: true, ai: false }, { id: "b", title: "PowerShell 7", kind: "windows", server: null, busy: false, ai: true }];
    return request("GET", "/api/terminals");
  },
  /** 请电脑打开一个终端（server 为服务器 ID，null 为本机终端）；电脑上开好后返回终端 ID */
  openTerminal: async (server: string | null): Promise<string> => {
    if (mock) { await sleep(800); return "a"; }
    return (await request<{ id: string }>("POST", "/api/terminals", { server }, 60_000)).id;
  },
  /** 读屏：最近 lines 行（往上翻时多要一些） */
  screen: async (id: string, lines = 300): Promise<Screen> => {
    if (mock) return mockScreen(id);
    return request("GET", `/api/terminals/${encodeURIComponent(id)}/screen?lines=${lines}`);
  },
  input: async (id: string, req: InputRequest): Promise<void> => {
    if (mock) { await sleep(300); if (!req.confirmed && req.text && /rm\s+-rf/.test(req.text)) throw new ApiError(409, { needs_confirm: true, lines: [req.text.trim()] }); return; }
    await request("POST", `/api/terminals/${encodeURIComponent(id)}/input`, { ...req, confirmed: !!req.confirmed });
  },
  /**
   * 按手机宽度显示：把电脑上的终端临时改成 cols×rows（20 秒内要再发一次续期，否则电脑端自动恢复）；
   * size 为 null 时马上恢复。返回 false 表示电脑上正有人在用这个终端，暂时不改
   */
  size: async (id: string, size: { cols: number; rows: number } | null): Promise<boolean> => {
    if (mock) return true;
    const r = await request<{ ok?: boolean } | null>("POST", `/api/terminals/${encodeURIComponent(id)}/size`, size ? { cols: size.cols, rows: size.rows } : { restore: true });
    return r?.ok !== false;
  },
  /** 发一张图片：电脑端存到终端所在的机器（服务器的 /tmp 或本机），再把路径粘贴进终端 */
  image: async (id: string, image: Blob): Promise<void> => {
    if (mock) { await sleep(1200); return; }
    const res = await fetchWithin(`/api/terminals/${encodeURIComponent(id)}/image`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": image.type || "application/octet-stream", "X-Pierlink": "1" },
      body: image,
    }, 60_000);
    if (!res.ok) {
      const text = await res.text();
      throw new ApiError(res.status, text ? (JSON.parse(text) as Record<string, unknown>) : null);
    }
  },
  confirms: async (): Promise<ConfirmItem[]> => (mock ? mockConfirms : request("GET", "/api/confirms")),
  /** 回答一条确认；返回 false 表示已经失效（电脑上回答了或超时） */
  answer: async (id: number, answer: "allow" | "deny"): Promise<boolean> => {
    if (mock) { await sleep(300); mockConfirms = mockConfirms.filter((c) => c.id !== id); return true; }
    const r = await request<{ ok?: boolean } | null>("POST", `/api/confirms/${id}`, { answer });
    return r?.ok !== false;
  },
  stopAll: async (): Promise<void> => { if (!mock) await request("POST", "/api/stop-all", {}); },
  activity: async (limit = 50): Promise<ActivityItem[]> => {
    if (mock) return [{ timeMs: Date.now() - 60000, entry: "gpu-dev", client: "Claude Code 2.1", tool: "run_command", target: "gpu-dev", summary: "npm run build", outcome: "ok" }];
    return request("GET", `/api/activity?limit=${limit}`);
  },
  /** 订阅变化通知：返回关闭函数。收到事件名后由调用方重新拉对应的数据 */
  events: (onEvent: (name: string) => void, onState: (connected: boolean) => void): (() => void) => {
    if (mock) { onState(true); return () => {}; }
    let source: EventSource | null = null;
    let closed = false;
    let retry: ReturnType<typeof setTimeout> | null = null;
    let dropped = false;
    const open = () => {
      source = new EventSource("/api/events");
      source.onopen = () => {
        onState(true);
        // 断过线：真的连上以后再让各页面重新拉一次（不补发断线期间的通知）
        if (dropped) { dropped = false; onEvent("reconnected"); }
      };
      source.onmessage = (e) => onEvent(String(e.data));
      for (const name of ["confirms", "terminals", "overview", "activity"]) source.addEventListener(name, () => onEvent(name));
      source.onerror = () => {
        onState(false);
        dropped = true;
        source?.close();
        if (!closed) retry = setTimeout(open, 3000);
      };
    };
    open();
    return () => { closed = true; if (retry) clearTimeout(retry); source?.close(); };
  },
};
