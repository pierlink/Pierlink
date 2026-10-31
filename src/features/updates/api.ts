import { Channel } from "@tauri-apps/api/core";
import { inTauri } from "../shell/TitleBar";

/** 有新版本时的信息 */
export interface UpdateInfo {
  version: string;
  currentVersion: string;
  /** 发版时写的更新说明 */
  notes: string | null;
  date: string | null;
}

/** 下载安装进度 */
export type UpdateProgress = { kind: "downloading"; downloaded: number; total: number | null } | { kind: "installing" };

export interface UpdatesApi {
  /** 检查新版本；没有返回 null */
  check(): Promise<UpdateInfo | null>;
  /** 下载、校验签名并安装。Windows 上安装程序启动后 Pierlink 会退出，这个 Promise 不会再返回 */
  install(onProgress: (p: UpdateProgress) => void): Promise<void>;
}

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 浏览器预览：没有新版本 */
const previewApi: UpdatesApi = {
  async check() { return null; },
  async install() { /* 预览里不安装 */ },
};

/** 取得更新接口：应用里用后端命令，浏览器预览用示例 */
export async function getUpdatesApi(): Promise<UpdatesApi> {
  if (!inTauri) return previewApi;
  const { commands } = await import("../../bindings/commands");
  return {
    async check() {
      const info = unwrap(await commands.updateCheck());
      return info ? { version: info.version, currentVersion: info.currentVersion, notes: info.notes ?? null, date: info.date ?? null } : null;
    },
    async install(onProgress) {
      const channel = new Channel<{ kind: "downloading"; downloaded: number | null; total?: number | null } | { kind: "installing" }>();
      channel.onmessage = (m) => onProgress(m.kind === "installing" ? m : { kind: "downloading", downloaded: m.downloaded ?? 0, total: m.total ?? null });
      unwrap(await commands.updateInstall(channel));
    },
  };
}
