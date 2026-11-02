import { Channel } from "@tauri-apps/api/core";
import { inTauri } from "../shell/TitleBar";

/** 有新版本时的信息 */
export interface UpdateInfo {
  version: string;
  currentVersion: string;
  /** 发版时写的更新说明 */
  notes: string | null;
  date: string | null;
  /** 这个版本已经下载好（点“安装更新”就能装） */
  downloaded: boolean;
}

/** 下载进度 */
export type UpdateProgress = { kind: "downloading"; downloaded: number; total: number | null };

export interface UpdatesApi {
  /** 检查新版本；没有返回 null */
  check(): Promise<UpdateInfo | null>;
  /** 下载新版本并核对签名（不安装）；返回下载好的版本号 */
  download(onProgress: (p: UpdateProgress) => void): Promise<string>;
  /** 安装已经下载好的新版本。Windows 上 Pierlink 会先关闭、安装后重新打开，这个 Promise 不会再返回 */
  install(): Promise<void>;
}

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 浏览器预览：没有新版本 */
const previewApi: UpdatesApi = {
  async check() { return null; },
  async download() { return ""; },
  async install() { /* 预览里不安装 */ },
};

/** 取得更新接口：应用里用后端命令，浏览器预览用示例 */
export async function getUpdatesApi(): Promise<UpdatesApi> {
  if (!inTauri) return previewApi;
  const { commands } = await import("../../bindings/commands");
  return {
    async check() {
      const info = unwrap(await commands.updateCheck());
      return info ? { version: info.version, currentVersion: info.currentVersion, notes: info.notes ?? null, date: info.date ?? null, downloaded: info.downloaded } : null;
    },
    async download(onProgress) {
      const channel = new Channel<{ kind: "downloading"; downloaded: number | null; total?: number | null }>();
      channel.onmessage = (m) => onProgress({ kind: "downloading", downloaded: m.downloaded ?? 0, total: m.total ?? null });
      return unwrap(await commands.updateDownload(channel));
    },
    async install() {
      unwrap(await commands.updateInstall());
    },
  };
}
