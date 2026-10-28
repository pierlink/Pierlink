import { inTauri } from "../shell/TitleBar";

/** 手机入口可以监听的一个本机地址 */
export interface MobileAddress {
  ip: string;
  /** tailscale / lan / loopback */
  kind: string;
  /** 网卡名 */
  name: string;
}

export interface MobileStatus {
  enabled: boolean;
  running: boolean;
  /** 选定的监听地址；没选时为 null */
  bind: string | null;
  port: number;
  /** 手机上打开的地址（运行中才有） */
  url: string | null;
  addresses: MobileAddress[];
  /** 开不起来的原因 */
  error: unknown;
}

export interface MobileInvite { code: string; expiresAtMs: number; url: string; qrSvg: string }
export interface MobilePermissions { view: boolean; input: boolean; confirm: boolean }
export interface MobileDevice { id: string; name: string; createdMs: number; lastSeenMs: number | null; permissions: MobilePermissions }
export interface PairRequest { id: number; name: string; ip: string }

export interface MobileApi {
  status(): Promise<MobileStatus>;
  /** 打开或关闭手机入口，改监听地址和端口 */
  configure(enabled: boolean, bind: string | null, port: number): Promise<MobileStatus>;
  invite(): Promise<MobileInvite>;
  devices(): Promise<MobileDevice[]>;
  setPermissions(id: string, p: MobilePermissions): Promise<void>;
  revoke(id: string): Promise<void>;
  revokeAll(): Promise<void>;
  /** 回答配对请求 */
  answerPair(id: number, allow: boolean): Promise<boolean>;
  onPairRequest(cb: (r: PairRequest) => void): Promise<() => void>;
  onPairClosed(cb: (id: number) => void): Promise<() => void>;
  onChanged(cb: () => void): Promise<() => void>;
}

/** 浏览器预览：示例数据 */
function previewApi(): MobileApi {
  let status: MobileStatus = {
    enabled: false, running: false, bind: null, port: 48600, url: null, error: null,
    addresses: [{ ip: "100.101.12.34", kind: "tailscale", name: "Tailscale" }, { ip: "192.168.1.23", kind: "lan", name: "以太网" }, { ip: "127.0.0.1", kind: "loopback", name: "本机" }],
  };
  let devices: MobileDevice[] = [{ id: "d1", name: "iPhone", createdMs: Date.now() - 86400000 * 3, lastSeenMs: Date.now() - 600000, permissions: { view: true, input: false, confirm: true } }];
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 29 29"><rect width="29" height="29" fill="#fff"/><path d="M1 1h7v7H1zM21 1h7v7h-7zM1 21h7v7H1z" fill="none" stroke="#000" stroke-width="1"/><rect x="11" y="11" width="7" height="7" fill="#000"/></svg>`;
  return {
    status: async () => status,
    configure: async (enabled, bind, port) => {
      status = { ...status, enabled, bind, port, running: enabled && !!bind, url: enabled && bind ? `http://${bind}:${port}/` : null };
      return status;
    },
    invite: async () => ({ code: "K7QX4M2P", expiresAtMs: Date.now() + 600000, url: `${status.url ?? "http://100.101.12.34:48600/"}?code=K7QX4M2P`, qrSvg: svg }),
    devices: async () => devices,
    setPermissions: async (id, p) => { devices = devices.map((d) => (d.id === id ? { ...d, permissions: p } : d)); },
    revoke: async (id) => { devices = devices.filter((d) => d.id !== id); },
    revokeAll: async () => { devices = []; },
    answerPair: async () => true,
    onPairRequest: async () => () => {},
    onPairClosed: async () => () => {},
    onChanged: async () => () => {},
  };
}

let api: Promise<MobileApi> | null = null;
/** 取手机入口的接口（Tauri 里用后端，浏览器预览用示例） */
export function getMobileApi(): Promise<MobileApi> {
  api ??= inTauri ? import("./tauriApi").then((m) => m.tauriMobileApi) : Promise.resolve(previewApi());
  return api;
}
