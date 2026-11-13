import { inTauri } from "../shell/TitleBar";
import { getFilesApi } from "./api";

/**
 * 默认的下载文件夹：系统“下载”文件夹里的 Pierlink（浏览器预览里给一个示例路径）。
 * @returns 完整路径
 */
export async function defaultDownloadDir(): Promise<string> {
  if (!inTauri) return "C:\\Users\\dev\\Downloads\\Pierlink";
  const { downloadDir, join } = await import("@tauri-apps/api/path");
  return join(await downloadDir(), "Pierlink");
}

/**
 * 现在的下载文件夹：设置里选过的，没选过时用默认的。
 * @param setting 设置里的下载文件夹（null 表示用默认的）
 * @returns 完整路径
 */
export async function downloadDirOf(setting: string | null): Promise<string> {
  return setting?.trim() ? setting : defaultDownloadDir();
}

/**
 * 准备好下载文件夹：没有就建好（下载、打开之前调用）。
 * @param setting 设置里的下载文件夹（null 表示用默认的）
 * @returns 完整路径
 */
export async function prepareDownloadDir(setting: string | null): Promise<string> {
  const dir = await downloadDirOf(setting);
  const api = await getFilesApi();
  try {
    await api.list({ kind: "local" }, dir);
  } catch {
    // 读不出来多半是还没有：建一个；真建不了（上一级也没了、没权限）时把建文件夹的错误交给调用方
    await api.createDir({ kind: "local" }, dir);
  }
  return dir;
}
