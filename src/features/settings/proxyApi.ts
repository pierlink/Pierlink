import { inTauri } from "../shell/TitleBar";

export type ProxyMode = "off" | "system" | "custom";
export type ProxyKind = "http" | "socks5";

/** 代理设置（和后端一致；自定义的协议、地址、端口在选别的方式时也保留） */
export interface ProxySettings {
  mode: ProxyMode;
  kind: ProxyKind;
  host: string;
  port: number | null;
  /** 自定义代理时不走代理的地址 */
  bypass: string[];
}

/** Windows 当前的代理设置 */
export type SystemProxy =
  | { kind: "none" }
  | { kind: "server"; protocol: ProxyKind; host: string; port: number }
  | { kind: "pac"; url: string };

/** 默认的不走代理名单：和后端 core 的 DEFAULT_BYPASS 一致（proxyApi.test.ts 会核对） */
export const DEFAULT_BYPASS = [
  "localhost", "127.0.0.0/8", "::1", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16",
  "100.64.0.0/10", "fc00::/7", "fe80::/10", "*.local", "<local>",
];

async function cmds() {
  return (await import("../../bindings/commands")).commands;
}

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 浏览器预览里保存的设置 */
let previewSaved: ProxySettings = { mode: "off", kind: "http", host: "", port: null, bypass: DEFAULT_BYPASS };

/** 后端返回的设置补全字段（旧版本存的可能缺） */
function complete(s: Partial<ProxySettings>): ProxySettings {
  return { mode: s.mode ?? "off", kind: s.kind ?? "http", host: s.host ?? "", port: s.port ?? null, bypass: s.bypass ?? DEFAULT_BYPASS };
}

/** 读代理设置 */
export async function getProxy(): Promise<ProxySettings> {
  if (!inTauri) return previewSaved;
  return complete(await (await cmds()).proxyGet());
}

/** 保存代理设置并马上生效；返回整理过的设置。地址、端口不对时抛出 proxy.* 错误 */
export async function saveProxy(settings: ProxySettings): Promise<ProxySettings> {
  if (!inTauri) {
    if (settings.mode === "custom" && !settings.host.trim()) throw { code: "proxy.invalid_host", params: { host: "" } };
    if (settings.mode === "custom" && !settings.port) throw { code: "proxy.invalid_port", params: {} };
    previewSaved = { ...settings, host: settings.host.trim() };
    return previewSaved;
  }
  return complete(unwrap(await (await cmds()).proxySet(settings)));
}

/** Windows 当前的代理设置（每次现读） */
export async function systemProxy(): Promise<SystemProxy> {
  if (!inTauri) return { kind: "server", protocol: "http", host: "127.0.0.1", port: 7890 };
  return await (await cmds()).proxySystem();
}

/** 按界面上当前填的设置测试代理（不保存）；返回毫秒 */
export async function testProxy(settings: ProxySettings): Promise<number> {
  if (!inTauri) {
    await new Promise((r) => setTimeout(r, 300));
    return 86;
  }
  return unwrap(await (await cmds()).proxyTest(settings));
}
