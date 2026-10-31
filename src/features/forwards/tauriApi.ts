import { listen } from "@tauri-apps/api/event";
import { commands } from "../../bindings/commands";
import type { ForwardRecord, ForwardsApi, ForwardStatus } from "./api";

/** 与 Rust 端 forwards.rs 的 FORWARDS_EVENT 一致 */
const FORWARDS_EVENT = "pierlink://forwards";

/** tauri-specta 对可能失败的命令返回 {status, data|error}；这里统一转成“成功返回数据、失败抛出错误” */
function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

export const tauriForwardsApi: ForwardsApi = {
  list: async (serverId) => unwrap(await commands.forwardList(serverId)) as ForwardRecord[],
  save: async (input) => unwrap(await commands.forwardSave(input)) as ForwardRecord,
  setEnabled: async (id, enabled) => unwrap(await commands.forwardSetEnabled(id, enabled)) as ForwardRecord,
  remove: async (id) => { unwrap(await commands.forwardDelete(id)); },
  onStatus: (cb) => listen<ForwardStatus[]>(FORWARDS_EVENT, (e) => cb(e.payload)),
};
