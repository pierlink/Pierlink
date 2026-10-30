import { commands } from "../../bindings/commands";
import type { SnippetsApi } from "./api";

/** tauri-specta 对可能失败的命令返回 {status, data|error}；这里统一转成“成功返回数据、失败抛出错误” */
function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

export const tauriSnippetsApi: SnippetsApi = {
  list: async () => unwrap(await commands.snippetList()),
  save: async (s) => unwrap(await commands.snippetSave(s)),
  remove: async (id) => { unwrap(await commands.snippetDelete(id)); },
};
