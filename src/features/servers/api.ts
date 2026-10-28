import { inTauri } from "../shell/TitleBar";
import type { ConversionPreviewDto_Serialize } from "../../bindings/commands";

/** 服务器记录（与后端 storage::Server 字段一致，只列界面用到的） */
export interface ServerRecord {
  id: string;
  name: string;
  groupName: string | null;
  favorite: boolean;
  source: "manual" | "ssh_config";
  sshAlias: string | null;
  host: string | null;
  port: number | null;
  username: string | null;
  authMethod: "key" | "agent" | "password" | "dedicated_key";
  identityFile: string | null;
  loginShell: string | null;
  /** 已保存密码（密码本身在 Windows 凭据管理器里） */
  hasPassword: boolean;
  notes: string;
  /** 连接方式：内置 SSH（默认）/ 系统 OpenSSH（按 ~/.ssh/config） */
  connectionEngine: "builtin" | "openssh";
  /** 为什么用系统 OpenSSH（导入时写的，或内置连不上后改用时写的） */
  engineNote: string | null;
}

/** 新建或修改服务器的表单 */
export interface ServerInputView {
  id: string | null;
  name: string;
  groupName: string | null;
  host: string;
  port: number;
  username: string | null;
  authMethod: "key" | "agent" | "password" | "dedicated_key";
  identityFile: string | null;
  loginShell: string | null;
  notes: string;
  connectionEngine: "builtin" | "openssh";
  /** 密码登录时要保存的密码（只交给后端存进凭据管理器）；空表示不修改 */
  password?: string | null;
  /** 清除已保存的密码 */
  clearPassword?: boolean;
}

/** 导入服务器经 `ssh -G` 解析出的实际参数（只读） */
export interface ResolvedView {
  hostname: string;
  user: string;
  port: number;
  identityFiles: string[];
  proxyJump: string | null;
  viaProxyCommand: boolean;
  hasForwards: boolean;
  hostKeyCheckWeakened: boolean;
}

/** 导入服务器转换成内置 SSH 的预览：会抄进去的字段、需要新建的跳板、不能转的原因 */
export type ConversionPreview = ConversionPreviewDto_Serialize;

export interface ServersApi {
  list(): Promise<ServerRecord[]>;
  /** 修改导入服务器在应用里的设置（连接参数以 ssh config 为准） */
  updateLink(id: string, groupName: string | null, notes: string): Promise<ServerRecord>;
  resolve(id: string): Promise<ResolvedView>;
  save(input: ServerInputView): Promise<ServerRecord>;
  remove(id: string): Promise<void>;
  setFavorite(id: string, favorite: boolean): Promise<void>;
  /** 只改连接方式（内置 SSH 连不上时“改用系统 OpenSSH 重试”）；note 是显示在服务器设置里的原因，null 清掉 */
  setEngine(id: string, engine: "builtin" | "openssh", note: string | null): Promise<ServerRecord>;
  /** 预览把导入的服务器转成内置 SSH（不改数据） */
  convertPreview(id: string): Promise<ConversionPreview>;
  /** 按刚才的预览转换：变成应用内添加、用内置 SSH 的服务器 */
  convert(id: string): Promise<ServerRecord>;
}

/** 服务器记录在列表以外的地方被改了（例如终端里“改用系统 OpenSSH 重试”）时发出的页面事件 */
const SERVERS_CHANGED = "pierlink:servers-changed";

/** 通知服务器列表重新读取 */
export function notifyServersChanged(): void {
  window.dispatchEvent(new Event(SERVERS_CHANGED));
}

/**
 * 订阅“服务器记录被改了”
 * @param fn 收到通知时调用
 * @returns 取消订阅的函数
 */
export function onServersChanged(fn: () => void): () => void {
  window.addEventListener(SERVERS_CHANGED, fn);
  return () => window.removeEventListener(SERVERS_CHANGED, fn);
}

/** 显示用地址：用户@主机[:端口]，导入的服务器显示别名 */
export function serverAddress(s: ServerRecord): string {
  if (s.source === "ssh_config") return s.sshAlias ?? s.name;
  const port = s.port && s.port !== 22 ? `:${s.port}` : "";
  return `${s.username ? `${s.username}@` : ""}${s.host ?? ""}${port}`;
}

/** 浏览器预览用：内存里的示例数据，刷新页面后恢复 */
function createPreviewApi(): ServersApi {
  let seq = 10;
  const base = { groupName: null, favorite: false, source: "manual" as const, sshAlias: null, port: 22, authMethod: "agent" as const, identityFile: null, loginShell: null, hasPassword: false, notes: "", connectionEngine: "builtin" as const, engineNote: null };
  let items: ServerRecord[] = [
    { ...base, id: "s1", name: "gpu-dev", host: "203.0.113.18", username: "dev", favorite: true },
    { ...base, id: "s2", name: "hk-web-01", host: "198.51.100.7", username: "deploy", favorite: true },
    { ...base, id: "s3", name: "build-arm", host: "203.0.113.40", username: "ci", groupName: "lab" },
    { ...base, id: "s4", name: "lab-nas", host: "192.0.2.12", username: "admin", groupName: "lab", port: 2222 },
    { ...base, id: "s5", name: "gpu-a100", source: "ssh_config", sshAlias: "gpu-a100", host: null, port: null, username: null, groupName: "ssh config" },
  ];
  return {
    async list() { return items.map((x) => ({ ...x })); },
    async save(input) {
      if (!input.name.trim()) throw { code: "server.invalid_field", params: { field: "name" } };
      if (!input.host.trim() || /[\s@;]/.test(input.host) || input.host.startsWith("-")) throw { code: "server.invalid_field", params: { field: "host" } };
      const rec: ServerRecord = {
        ...base, ...(items.find((x) => x.id === input.id) ?? {}),
        id: input.id ?? `s${++seq}`, name: input.name.trim(), groupName: input.groupName?.trim() || null, host: input.host.trim(), port: input.port,
        username: input.username?.trim() || null, authMethod: input.authMethod, identityFile: input.authMethod === "key" ? input.identityFile : null,
        loginShell: input.loginShell?.trim() || null, notes: input.notes, connectionEngine: input.connectionEngine,
        hasPassword: input.authMethod === "password" && (input.password ? true : input.clearPassword ? false : (items.find((x) => x.id === input.id)?.hasPassword ?? false)),
      };
      items = input.id ? items.map((x) => (x.id === input.id ? rec : x)) : [...items, rec];
      return rec;
    },
    async remove(id) {
      if (id === "s1") throw { code: "storage.server_in_use", params: { task_names: JSON.stringify(["web-api", "ml-train"]) } };
      items = items.filter((x) => x.id !== id);
    },
    async setFavorite(id, favorite) { items = items.map((x) => (x.id === id ? { ...x, favorite } : x)); },
    async setEngine(id, engine, note) {
      items = items.map((x) => (x.id === id ? { ...x, connectionEngine: engine, engineNote: note } : x));
      return items.find((x) => x.id === id)!;
    },
    async updateLink(id, groupName, notes) {
      items = items.map((x) => (x.id === id ? { ...x, groupName: groupName?.trim() || null, notes } : x));
      return items.find((x) => x.id === id)!;
    },
    async convertPreview(id) {
      await new Promise((r) => setTimeout(r, 300));
      const s = items.find((x) => x.id === id);
      return {
        target: { alias: s?.sshAlias ?? id, host: "203.0.113.21", port: 22, username: "ml", identityFiles: ["~/.ssh/id_ed25519"], identitiesOnly: false, hostKeyAlias: null },
        jumps: [{ fields: { alias: "bastion", host: "203.0.113.5", port: 22, username: "ml", identityFiles: [], identitiesOnly: false, hostKeyAlias: null }, existingServerId: null }],
        reasons: [],
      };
    },
    async convert(id) {
      items = items.map((x) => (x.id === id ? { ...x, source: "manual", sshAlias: null, host: "203.0.113.21", port: 22, username: "ml", connectionEngine: "builtin", engineNote: null } : x));
      return items.find((x) => x.id === id)!;
    },
    async resolve() {
      await new Promise((r) => setTimeout(r, 200));
      return { hostname: "203.0.113.21", user: "ml", port: 22, identityFiles: ["~/.ssh/id_ed25519"], proxyJump: "bastion", viaProxyCommand: false, hasForwards: true, hostKeyCheckWeakened: false };
    },
  };
}

let api: ServersApi | null = null;

/** 取得服务器接口：Tauri 里用后端，浏览器里用示例数据 */
export async function getServersApi(): Promise<ServersApi> {
  if (api) return api;
  if (inTauri) {
    const { createTauriServersApi } = await import("./tauriApi");
    api = createTauriServersApi();
  } else {
    api = createPreviewApi();
  }
  return api;
}
