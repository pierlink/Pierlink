/**
 * 手机页面和 Windows 上手机入口之间的接口。同源请求、设备凭据在 HttpOnly Cookie 里；
 * POST 都带 X-Pierlink 头（入口据此拒绝跨站请求）。地址里带 ?mock 时用示例数据（开发预览）。
 */

export interface Me { name: string; permissions: { view: boolean; input: boolean; confirm: boolean } }
export interface OverviewServer { id: string; name: string; online: boolean | null; cpu: number | null; mem: number | null }
export interface OverviewSync { id: string; name: string; state: string; percent: number | null }
export interface Overview { servers: OverviewServer[]; sync: OverviewSync[]; ai: { sessions: number; pending: number } }
export interface TerminalItem { id: string; title: string; kind: "windows" | "server"; server: string | null; busy: boolean | null; ai: boolean }
export interface Screen { text: string; rows: number; cols: number; password: boolean }
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

async function request<T>(method: "GET" | "POST", path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    credentials: "same-origin",
    headers: method === "POST" ? { "Content-Type": "application/json", "X-Pierlink": "1" } : undefined,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
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

export const api = {
  me: async (): Promise<Me> => {
    if (mock) { if (!mockPaired && !location.search.includes("paired")) throw new ApiError(401, null); return { name: "iPhone", permissions: { view: true, input: true, confirm: true } }; }
    return request("GET", "/api/me");
  },
  pair: async (code: string, deviceName: string): Promise<void> => {
    if (mock) { await sleep(1500); if (code.length !== 8) throw new ApiError(401, { code: "mobile.invalid_invite" }); mockPaired = true; return; }
    await request("POST", "/api/pair", { code, device_name: deviceName });
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
  screen: async (id: string): Promise<Screen> => {
    if (mock) return { text: `dev@gpu-dev:~/web-api$ npm run build\n\n> web-api@1.0.0 build\n> tsc -p .\n\nsrc/routes/order.ts:12:5 - error TS2322\n\ndev@gpu-dev:~/web-api$ ${id === "a" ? "" : ""}`, rows: 24, cols: 80, password: false };
    return request("GET", `/api/terminals/${encodeURIComponent(id)}/screen`);
  },
  input: async (id: string, text: string | null, keys: string[], confirmed = false): Promise<void> => {
    if (mock) { await sleep(300); if (!confirmed && text && /rm\s+-rf/.test(text)) throw new ApiError(409, { needs_confirm: true, lines: [text.trim()] }); return; }
    // 入口要求文字和按键二选一
    await request("POST", `/api/terminals/${encodeURIComponent(id)}/input`, text ? { text, confirmed } : { keys, confirmed });
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
    const open = () => {
      source = new EventSource("/api/events");
      source.onopen = () => onState(true);
      source.onmessage = (e) => onEvent(String(e.data));
      for (const name of ["confirms", "terminals", "overview", "activity"]) source.addEventListener(name, () => onEvent(name));
      source.onerror = () => {
        onState(false);
        source?.close();
        // 断线后过一会儿再连；不补发历史，连上后各页面重新拉一次
        if (!closed) retry = setTimeout(() => { open(); onEvent("reconnected"); }, 3000);
      };
    };
    open();
    return () => { closed = true; if (retry) clearTimeout(retry); source?.close(); };
  },
};
