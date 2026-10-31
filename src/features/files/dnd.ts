import { useSyncExternalStore, type PointerEvent as ReactPointerEvent } from "react";
import type { Loc } from "./types";

/**
 * 文件面板之间的拖放。用指针事件自己实现（不用 HTML5 拖放）：
 * Windows 的 WebView2 在开启系统文件拖入时会吞掉网页内部的拖放事件，自己实现行为在各平台一致。
 */

/** 被拖动的文件：来自哪个窗格、哪台机器的哪个目录 */
export interface DragPayload {
  paneId: string;
  loc: Loc;
  /** 来源机器的家目录（判断同步任务里 ~ 开头的路径用） */
  home: string | null;
  dir: string;
  names: string[];
}

/** 拖到某个面板上时，面板提供的目标信息 */
export interface DropTarget {
  paneId: string;
  loc: Loc;
  home: string | null;
  dir: string;
  title: string;
}

interface DragState { payload: DragPayload; x: number; y: number; over: string | null }

let state: DragState | null = null;
const listeners = new Set<() => void>();
const targets = new Map<string, { target: () => DropTarget | null; drop: (p: DragPayload, t: DropTarget) => void }>();
const emit = () => listeners.forEach((l) => l());
const subscribe = (l: () => void) => { listeners.add(l); return () => listeners.delete(l); };

/** 当前拖动状态（没在拖时为 null） */
export function useDragState(): DragState | null {
  return useSyncExternalStore(subscribe, () => state);
}

/**
 * 登记一个可放置的面板。
 * @param target 返回当前目录等信息；还没打开目录时返回 null（不能放）
 * @param drop 放下时调用
 * @returns 取消登记
 */
export function registerDropTarget(paneId: string, target: () => DropTarget | null, drop: (p: DragPayload, t: DropTarget) => void): () => void {
  targets.set(paneId, { target, drop });
  return () => { if (targets.get(paneId)?.drop === drop) targets.delete(paneId); };
}

/** 指针下面的面板 ID（面板根元素带 data-file-drop） */
function paneAt(x: number, y: number): string | null {
  const el = document.elementFromPoint(x, y)?.closest<HTMLElement>("[data-file-drop]");
  return el?.dataset.fileDrop ?? null;
}

/** 某个面板能否接收：不是来源面板，且目标已经打开了目录 */
export function dropTargetInfo(paneId: string | null, payload: DragPayload): DropTarget | null {
  if (!paneId || paneId === payload.paneId) return null;
  return targets.get(paneId)?.target() ?? null;
}

/**
 * 在文件行上按下鼠标后调用：移动超过几个像素才算开始拖动（否则是普通点击）。
 * Esc 取消拖动。
 */
export function beginDrag(e: ReactPointerEvent, payload: () => DragPayload | null): void {
  if (e.button !== 0) return;
  const startX = e.clientX;
  const startY = e.clientY;
  let started = false;
  const move = (ev: PointerEvent) => {
    if (!started) {
      if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < 6) return;
      const p = payload();
      if (!p) { cleanup(); return; }
      started = true;
      state = { payload: p, x: ev.clientX, y: ev.clientY, over: null };
      document.body.classList.add("files-dragging");
    }
    if (!state) return;
    state = { ...state, x: ev.clientX, y: ev.clientY, over: paneAt(ev.clientX, ev.clientY) };
    emit();
  };
  const up = (ev: PointerEvent) => {
    const s = state;
    cleanup();
    if (!started || !s) return;
    const over = paneAt(ev.clientX, ev.clientY);
    const target = dropTargetInfo(over, s.payload);
    if (over && target) targets.get(over)?.drop(s.payload, target);
  };
  const key = (ev: KeyboardEvent) => { if (ev.key === "Escape") cleanup(); };
  const cleanup = () => {
    window.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", up);
    window.removeEventListener("pointercancel", cleanup);
    window.removeEventListener("keydown", key, true);
    document.body.classList.remove("files-dragging");
    if (state) { state = null; emit(); }
  };
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up);
  window.addEventListener("pointercancel", cleanup);
  window.addEventListener("keydown", key, true);
}
