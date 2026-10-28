import { commands } from "../../bindings/commands";
import type { ServerInputView, ServersApi } from "./api";

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 运行在 Tauri 窗口里时使用的服务器接口（后端返回的字段与界面记录一致） */
export function createTauriServersApi(): ServersApi {
  return {
    async list() { return unwrap(await commands.serverList()); },
    async save(input: ServerInputView) {
      // 密码单独传：不放进服务器记录，后端只存进凭据管理器
      const { password, clearPassword, ...rest } = input;
      return unwrap(await commands.serverSave(rest, password || null, !!clearPassword));
    },
    async remove(id) { unwrap(await commands.serverDelete(id)); },
    async setFavorite(id, favorite) { unwrap(await commands.serverSetFavorite(id, favorite)); },
    async setEngine(id, engine, note) { return unwrap(await commands.serverSetEngine(id, engine, note)); },
    async updateLink(id, groupName, notes) { return unwrap(await commands.serverUpdateLink(id, groupName, notes)); },
    async resolve(id) { return unwrap(await commands.serverResolve(id)); },
    async convertPreview(id) { return unwrap(await commands.serverConvertPreview(id)); },
    async convert(id) { return unwrap(await commands.serverConvert(id)); },
  };
}
