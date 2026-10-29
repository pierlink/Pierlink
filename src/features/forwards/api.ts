import { inTauri } from "../shell/TitleBar";

export type ForwardKind = "local" | "remote" | "dynamic";
/** waiting_password：这台服务器要密码登录，本次运行还没输过或输的不对，等用户输入 */
export type ForwardState = "stopped" | "connecting" | "running" | "reconnecting" | "waiting_password" | "error";

/** 一条转发现在的状态 */
export interface ForwardStatus {
  id: string;
  state: ForwardState | string;
  /** 最近一次错误（重连中也可能有，说明为什么断了） */
  error: { code: string; params: Record<string, string> } | null;
  nextRetryAtMs: number | null;
}

/**
 * 一条端口转发。
 * local：本机 127.0.0.1:localPort → 经服务器到 remoteHost:remotePort；
 * remote：服务器 127.0.0.1:remotePort → 本机 127.0.0.1:localPort；
 * dynamic：本机 127.0.0.1:localPort 上的 SOCKS 代理。
 */
export interface ForwardRecord {
  id: string;
  serverId: string;
  name: string;
  kind: ForwardKind;
  localPort: number;
  remoteHost: string | null;
  remotePort: number | null;
  /** 已启用：现在开着，应用启动时也自动开启 */
  enabled: boolean;
  sortOrder: number;
  status: ForwardStatus;
}

export type ForwardInput = Omit<ForwardRecord, "id" | "status"> & { id: string | null };

/** 端口转发接口：Tauri 实现和浏览器预览实现都遵守它 */
export interface ForwardsApi {
  list(serverId: string): Promise<ForwardRecord[]>;
  save(input: ForwardInput): Promise<ForwardRecord>;
  setEnabled(id: string, enabled: boolean): Promise<ForwardRecord>;
  remove(id: string): Promise<void>;
  /** 订阅状态变化；返回取消订阅的函数 */
  onStatus(cb: (list: ForwardStatus[]) => void): Promise<() => void>;
}

/** 浏览器预览：存在内存里，开启后过一会儿变成“运行中” */
function previewApi(): ForwardsApi {
  let items: ForwardRecord[] = [];
  const listeners = new Set<(l: ForwardStatus[]) => void>();
  const status = (id: string, state: ForwardState): ForwardStatus => ({ id, state, error: null, nextRetryAtMs: null });
  const push = (s: ForwardStatus) => {
    items = items.map((x) => (x.id === s.id ? { ...x, status: s } : x));
    for (const l of listeners) l([s]);
  };
  let seq = 0;
  const start = (id: string) => {
    push(status(id, "connecting"));
    setTimeout(() => { if (items.find((x) => x.id === id)?.enabled) push(status(id, "running")); }, 800);
  };
  return {
    list: async (serverId) => items.filter((x) => x.serverId === serverId).map((x) => ({ ...x })),
    save: async (input) => {
      const rec: ForwardRecord = { ...input, id: input.id ?? `f${++seq}`, status: status(input.id ?? "", "stopped") };
      rec.status = status(rec.id, "stopped");
      items = items.some((x) => x.id === rec.id) ? items.map((x) => (x.id === rec.id ? rec : x)) : [...items, rec];
      if (rec.enabled) start(rec.id);
      return rec;
    },
    setEnabled: async (id, enabled) => {
      items = items.map((x) => (x.id === id ? { ...x, enabled } : x));
      if (enabled) start(id); else push(status(id, "stopped"));
      return { ...items.find((x) => x.id === id)! };
    },
    remove: async (id) => { items = items.filter((x) => x.id !== id); },
    onStatus: async (cb) => { listeners.add(cb); return () => listeners.delete(cb); },
  };
}

let api: Promise<ForwardsApi> | null = null;
/** 取得端口转发接口（Tauri 里用应用后端，浏览器预览用内存实现） */
export function getForwardsApi(): Promise<ForwardsApi> {
  api ??= inTauri ? import("./tauriApi").then((m) => m.tauriForwardsApi) : Promise.resolve(previewApi());
  return api;
}

/**
 * 转发的映射说明，如“本机 127.0.0.1:6006 → 服务器 127.0.0.1:6006”。
 * @param f 转发
 * @param t 翻译函数
 */
export function describeForward(f: Pick<ForwardRecord, "kind" | "localPort" | "remoteHost" | "remotePort">, t: (k: string, o?: Record<string, unknown>) => string): string {
  if (f.kind === "local") return t("forwards.mapLocal", { local: f.localPort, host: f.remoteHost ?? "127.0.0.1", port: f.remotePort ?? "" });
  if (f.kind === "remote") return t("forwards.mapRemote", { remote: f.remotePort ?? "", local: f.localPort });
  return t("forwards.mapDynamic", { local: f.localPort });
}
