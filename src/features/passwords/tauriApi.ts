import { listen } from "@tauri-apps/api/event";
import { commands, type PasswordRequestDto_Deserialize } from "../../bindings/commands";
import type { PasswordRequest, PasswordsApi } from "./api";

/** 与 Rust 端 passwords.rs 的事件名一致 */
const REQUEST_EVENT = "pierlink://password-request";
const CLOSED_EVENT = "pierlink://password-closed";
const SAVED_EVENT = "pierlink://password-saved";

/** tauri-specta 对可能失败的命令返回 {status, data|error}；这里统一转成“成功返回数据、失败抛出错误” */
function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

const KINDS = ["password", "other", "confirm", "notice"] as const;

function toRequest(r: PasswordRequestDto_Deserialize): PasswordRequest {
  const kind = (KINDS as readonly string[]).includes(r.kind) ? (r.kind as PasswordRequest["kind"]) : "other";
  return { id: r.id ?? 0, serverId: r.serverId, serverName: r.serverName, kind, prompt: r.prompt ?? null, purpose: r.purpose, canRemember: r.canRemember };
}

export const tauriPasswordsApi: PasswordsApi = {
  pending: async () => unwrap(await commands.passwordPending()).map(toRequest),
  answer: async (id, password, remember) => unwrap(await commands.passwordAnswer(id, password, remember)),
  enter: async (serverId, password, remember) => { unwrap(await commands.passwordEnter(serverId, password, remember)); },
  onRequest: (cb) => listen<PasswordRequestDto_Deserialize>(REQUEST_EVENT, (e) => cb(toRequest(e.payload))),
  onClosed: (cb) => listen<number>(CLOSED_EVENT, (e) => cb(e.payload)),
  onSaved: (cb) => listen<string>(SAVED_EVENT, (e) => cb(e.payload)),
};
