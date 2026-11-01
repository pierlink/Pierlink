import { useSyncExternalStore } from "react";

/** “隐藏 IP”的开关记在本机（每台电脑自己决定） */
const KEY = "pierlink.hideAddresses";
/** 遮住地址用的圆点 */
export const MASK = "•••••";

let hidden = read();
const listeners = new Set<() => void>();

function read(): boolean {
  try {
    return localStorage.getItem(KEY) === "1";
  } catch {
    return false;
  }
}

/**
 * 打开或关闭“隐藏 IP”：只是界面上看不见（防止截图泄露），复制照样复制真实地址。
 * @param value true 表示隐藏
 */
export function setHideAddresses(value: boolean): void {
  hidden = value;
  try {
    localStorage.setItem(KEY, value ? "1" : "0");
  } catch {
    // 存不了就只在这次运行里有效
  }
  for (const l of listeners) l();
}

function subscribe(l: () => void): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}

/** 现在是不是隐藏 IP（切换时组件跟着刷新） */
export function useHideAddresses(): boolean {
  return useSyncExternalStore(subscribe, () => hidden);
}

const IPV4 = /\b(?:\d{1,3}\.){3}\d{1,3}\b/g;

/**
 * 把文字里的服务器地址换成圆点：这台服务器的主机名或 IP（不分大小写；前后紧挨着字母或数字的不算，
 * 主机 dev 不会把 device-lab 遮掉一截），以及任何像 IPv4 地址的部分。
 * @param text 要显示的文字（名称、user@host:port 等）
 * @param host 这台服务器的主机（不知道时为 null）
 * @returns 遮住地址后的文字
 */
export function maskHost(text: string, host: string | null | undefined): string {
  const h = host?.trim();
  const out = h ? text.replace(new RegExp(`(?<![A-Za-z0-9])${h.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?![A-Za-z0-9])`, "gi"), MASK) : text;
  return out.replace(IPV4, MASK);
}
