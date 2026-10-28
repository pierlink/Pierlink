import type { EntrySettings } from "./api";

/**
 * 规范化用户输入的文件夹：去掉两边空白和包住整段的引号，去掉末尾的斜杠（盘符根目录保留成 C:\）。
 * @param folder 用户输入
 * @returns 规范化后的路径；输入为空时为空字符串
 */
export function normalizeFolder(folder: string): string {
  let f = folder.trim().replace(/^"(.*)"$/, "$1").trim();
  // Windows 路径（盘符开头）统一用反斜杠：D:/work 和 D:\work 是同一个文件夹
  if (/^[A-Za-z]:/.test(f)) f = f.replace(/\//g, "\\");
  if (/^[A-Za-z]:[\\/]?$/.test(f)) return `${f.slice(0, 2)}\\`;
  return f.replace(/[\\/]+$/, "");
}

/**
 * 加一个允许的文件夹；已有（不区分大小写，Windows 路径）或为空时原样返回。
 * @param entry 入口设置
 * @param folder 用户输入的文件夹
 * @returns 新的入口设置
 */
export function addFolder(entry: EntrySettings, folder: string): EntrySettings {
  const f = normalizeFolder(folder);
  if (!f || entry.folders.some((x) => normalizeFolder(x).toLowerCase() === f.toLowerCase())) return entry;
  return { ...entry, folders: [...entry.folders, f] };
}

/**
 * 去掉一个允许的文件夹。
 * @param entry 入口设置
 * @param folder 要去掉的文件夹（原文）
 * @returns 新的入口设置
 */
export function removeFolder(entry: EntrySettings, folder: string): EntrySettings {
  return { ...entry, folders: entry.folders.filter((x) => x !== folder) };
}

/**
 * 勾选或取消一台允许操作的服务器。
 * @param entry 入口设置
 * @param serverId 服务器 ID
 * @returns 新的入口设置
 */
export function toggleServer(entry: EntrySettings, serverId: string): EntrySettings {
  return entry.servers.includes(serverId)
    ? { ...entry, servers: entry.servers.filter((x) => x !== serverId) }
    : { ...entry, servers: [...entry.servers, serverId] };
}

/**
 * AI 在本机打开终端时用的 shell：优先 PowerShell 7，其次 Windows PowerShell（命令写法确定，切目录用 Set-Location）。
 * @param shells 检测到的本机 shell
 * @returns 选中的 shell；一个 PowerShell 都没有时为 undefined
 */
export function pickPowerShell<T extends { id: string; name: string }>(shells: T[]): T | undefined {
  const by = (re: RegExp) => shells.find((s) => re.test(s.id) || re.test(s.name));
  return by(/pwsh|PowerShell 7/i) ?? by(/powershell/i);
}
