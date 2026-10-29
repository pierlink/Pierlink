import { useSyncExternalStore } from "react";
import { inTauri } from "../shell/TitleBar";

/**
 * 弹到独立窗口里的服务器桌面。每台服务器最多一个，窗口标签是 desktop-服务器ID：
 * 再打开同一台服务器的桌面时把已有的窗口提到前面，侧栏据此显示“桌面在独立窗口里”的标识。
 */

const PREFIX = "desktop-";
let popped = new Set<string>();
const listeners = new Set<() => void>();

function setPopped(serverId: string, open: boolean) {
  if (popped.has(serverId) === open) return;
  popped = new Set(popped);
  if (open) popped.add(serverId); else popped.delete(serverId);
  listeners.forEach((l) => l());
}

/** 窗口关掉时从列表里去掉 */
function watch(win: { once: (event: string, cb: () => void) => Promise<unknown> }, serverId: string) {
  void win.once("tauri://destroyed", () => setPopped(serverId, false));
}

let started = false;
/** 应用里已经开着的桌面窗口（主窗口重新加载后也能接上） */
function start() {
  if (started || !inTauri) return;
  started = true;
  void import("@tauri-apps/api/webviewWindow").then(async ({ getAllWebviewWindows }) => {
    for (const w of await getAllWebviewWindows()) {
      if (!w.label.startsWith(PREFIX)) continue;
      const id = w.label.slice(PREFIX.length);
      setPopped(id, true);
      watch(w, id);
    }
  }).catch(() => undefined);
}

/** 当前有独立桌面窗口的服务器 ID（变化时组件会重新渲染） */
export function useDesktopPopouts(): Set<string> {
  start();
  return useSyncExternalStore((cb) => { listeners.add(cb); return () => listeners.delete(cb); }, () => popped);
}

/**
 * 把这台服务器已经开着的桌面窗口提到前面。
 * @returns 有这个窗口时返回 true
 */
export async function focusDesktopWindow(serverId: string): Promise<boolean> {
  if (!inTauri) return false;
  const { WebviewWindow } = await import("@tauri-apps/api/webviewWindow");
  const win = await WebviewWindow.getByLabel(PREFIX + serverId);
  if (!win) { setPopped(serverId, false); return false; }
  await win.unminimize().catch(() => undefined);
  await win.setFocus().catch(() => undefined);
  return true;
}

/**
 * 在独立窗口里打开这台服务器的桌面；已经开着就提到前面。
 * @param serverId 服务器 ID
 * @param title 窗口标题
 * @param url 窗口地址
 * @returns 窗口开出来（或已有）时返回 true；8 秒后还不见窗口返回 false；创建出错时抛出原因
 */
export async function openDesktopWindow(serverId: string, title: string, url: string): Promise<boolean> {
  if (!inTauri) { window.open(url.replace(/^index\.html/, "/"), "_blank"); return true; }
  if (await focusDesktopWindow(serverId)) return true;
  const { WebviewWindow } = await import("@tauri-apps/api/webviewWindow");
  const label = PREFIX + serverId;
  const win = new WebviewWindow(label, { url, title, width: 1280, height: 800, minWidth: 640, minHeight: 480 });
  return new Promise((resolve, reject) => {
    let settled = false;
    const done = (ok: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (ok) { setPopped(serverId, true); watch(win, serverId); }
      resolve(ok);
    };
    // 迟迟等不到“已创建”的通知时，直接查一下窗口在不在，不让界面一直卡着
    const timer = setTimeout(() => {
      void WebviewWindow.getByLabel(label).then((w) => done(!!w), () => done(false));
    }, 8000);
    void win.once("tauri://created", () => done(true));
    void win.once("tauri://error", (e) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      reject(new Error(String(e.payload ?? "创建窗口失败")));
    });
  });
}
