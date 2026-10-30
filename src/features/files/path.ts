import type { Loc } from "./types";

/** 是否是 Windows 路径（盘符开头或 \\ 网络路径） */
export function isWindowsPath(path: string): boolean {
  return /^[A-Za-z]:([\\/]|$)/.test(path) || path.startsWith("\\\\");
}

/** 按路径风格拼接目录和名字 */
export function joinPath(dir: string, name: string): string {
  if (isWindowsPath(dir)) return dir.endsWith("\\") || dir.endsWith("/") ? `${dir}${name}` : `${dir}\\${name}`;
  return dir.endsWith("/") ? `${dir}${name}` : `${dir}/${name}`;
}

/**
 * 上一级目录。
 * @returns 上一级；已经是根（`/`、`C:\`）时返回 null
 */
export function parentPath(path: string): string | null {
  if (isWindowsPath(path)) {
    const p = path.replace(/[\\/]+$/, "");
    if (/^[A-Za-z]:$/.test(p)) return null;
    const i = Math.max(p.lastIndexOf("\\"), p.lastIndexOf("/"));
    if (i < 0) return null;
    const up = p.slice(0, i);
    return /^[A-Za-z]:$/.test(up) ? `${up}\\` : up || null;
  }
  const p = path.replace(/\/+$/, "");
  if (!p) return null;
  const i = p.lastIndexOf("/");
  return i <= 0 ? "/" : p.slice(0, i);
}

/** 路径最后一段（根目录返回原样） */
export function baseName(path: string): string {
  const p = path.replace(/[\\/]+$/, "");
  const i = Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"));
  return i >= 0 ? p.slice(i + 1) || path : p || path;
}

/** 面包屑：从根到当前目录的每一级（名字、完整路径） */
export function crumbs(path: string): { name: string; path: string }[] {
  const out: { name: string; path: string }[] = [];
  let p: string | null = path;
  while (p) {
    out.unshift({ name: parentPath(p) === null ? p.replace(/[\\/]+$/, "") || "/" : baseName(p), path: p });
    p = parentPath(p);
  }
  return out;
}

/** 规范化成比较用的形式：统一分隔符、去掉结尾分隔符；Windows 路径不区分大小写 */
function normalize(path: string): string {
  if (isWindowsPath(path)) return path.replace(/\//g, "\\").replace(/\\+$/, "").toLowerCase();
  return path.replace(/\/+$/, "") || "/";
}

/**
 * path 相对 root 的部分。
 * @returns 相对路径（统一用 / 分隔，相同时为空串）；不在 root 下返回 null
 */
export function relativeTo(path: string, root: string): string | null {
  const p = normalize(path);
  const r = normalize(root);
  if (p === r) return "";
  const sep = isWindowsPath(path) ? "\\" : "/";
  const prefix = r.endsWith(sep) ? r : `${r}${sep}`;
  if (!p.startsWith(prefix)) return null;
  return p.slice(prefix.length).split(sep).join("/");
}

/** 两个位置是不是同一台机器 */
export function sameLoc(a: Loc, b: Loc): boolean {
  if (a.kind === "local" || b.kind === "local") return a.kind === b.kind;
  if (a.kind === "server" && b.kind === "server") return a.serverId === b.serverId;
  if (a.kind === "quick" && b.kind === "quick") return a.host === b.host && a.port === b.port && a.user === b.user;
  return false;
}

/** 位置的唯一键（刷新通知、缓存用） */
export function locKey(loc: Loc): string {
  if (loc.kind === "local") return "local";
  if (loc.kind === "server") return `server:${loc.serverId}`;
  return `quick:${loc.user ?? ""}@${loc.host}:${loc.port}`;
}

/** 同步任务里和判断有关的部分 */
export interface SyncPair { name: string; localPath: string; serverId: string; remotePath: string }

/**
 * 两个目录是不是已经由同步任务连在一起（本机目录和服务器目录在同一个任务里、相对位置相同）。
 * 这种情况下拖过去传输没有意义，文件本来就会同步过去。
 * @param remoteHome 服务器的家目录（任务里的远程路径可能用 ~ 开头）
 * @returns 覆盖这两个目录的任务名；没有返回 null
 */
export function syncCovering(pairs: SyncPair[], a: { loc: Loc; dir: string }, b: { loc: Loc; dir: string }, remoteHome: string | null): string | null {
  const local = a.loc.kind === "local" ? a : b.loc.kind === "local" ? b : null;
  const remote = a.loc.kind === "server" ? a : b.loc.kind === "server" ? b : null;
  if (!local || !remote || remote.loc.kind !== "server") return null;
  const serverId = remote.loc.serverId;
  for (const p of pairs) {
    if (p.serverId !== serverId) continue;
    let root = p.remotePath;
    if (root === "~" || root.startsWith("~/")) {
      if (!remoteHome) continue;
      root = remoteHome.replace(/\/+$/, "") + root.slice(1);
    }
    const l = relativeTo(local.dir, p.localPath);
    const r = relativeTo(remote.dir, root);
    if (l !== null && r !== null && l === r) return p.name;
  }
  return null;
}

/** 字节数显示成 KB/MB/GB */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}
