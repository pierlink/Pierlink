import { commands } from "../../bindings/commands";
import type { ImportApi, ImportPreview } from "./importApi";

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 运行在 Tauri 窗口里时使用的导入接口 */
export function createTauriImportApi(): ImportApi {
  return {
    async preview() { return unwrap(await commands.sshImportPreview()) as ImportPreview; },
    async cancel() { await commands.sshImportCancel(); },
    async importAliases(aliases, groupName) { return unwrap(await commands.sshImport(aliases, groupName)); },
  };
}
