import { useSyncExternalStore } from "react";

/** 连接服务器时是否自动显示概况（各窗格共用，存在本机） */
const KEY = "pierlink.overview.visible";
const listeners = new Set<() => void>();

export function overviewAuto(): boolean {
  try { return localStorage.getItem(KEY) !== "0"; } catch { return true; }
}

export function setOverviewAuto(on: boolean) {
  try { localStorage.setItem(KEY, on ? "1" : "0"); } catch { /* 存不了就只在这次有效 */ }
  listeners.forEach((f) => f());
}

/** 读这个开关，变了自动刷新 */
export function useOverviewAuto(): boolean {
  return useSyncExternalStore((f) => { listeners.add(f); return () => listeners.delete(f); }, overviewAuto);
}
