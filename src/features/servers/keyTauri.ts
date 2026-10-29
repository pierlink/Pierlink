import { commands } from "../../bindings/commands";
import type { KeyApi, KeyInstallResult, KeyStatus } from "./keyApi";

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 运行在 Tauri 窗口里时使用的专用密钥接口 */
export function createTauriKeyApi(): KeyApi {
  return {
    async status(id) { return unwrap(await commands.keyStatus(id)) as KeyStatus; },
    async install(id, password) { return unwrap(await commands.keyInstall(id, password)) as KeyInstallResult; },
    async remove(id, line) { return unwrap(await commands.keyRemove(id, line)) as "removed" | "notFound" | "symlink"; },
  };
}
