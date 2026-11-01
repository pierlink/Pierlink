import { useSyncExternalStore, type PointerEvent as ReactPointerEvent } from "react";
import { baseName, parentPath } from "./path";
import type { Loc } from "./types";

/**
 * 文件面板之间的拖放。用指针事件自己实现（不用 HTML5 拖放）：
 * Windows 的 WebView2 在开启系统文件拖入时会吞掉网页内部的拖放事件，自己实现行为在各平台一致。
 * 服务器上的文件拖出窗口时交给系统拖放（拖到资源管理器等，松手时才下载），见 beginDrag 的 out 参数。
 */

/** 被拖动的文件：来自哪个窗格、哪台机器的哪个目录 */
export interface DragPayload {
  paneId: string;
  loc: Loc;
  /** 来源机器的家目录（判断同步任务里 ~ 开头的路径用） */
  home: string | null;
  dir: string;
  names: string[];
  /** 每一项的完整路径（从资源管理器拖进来的文件可能不在同一个目录）；没有时按 dir + names */
  paths?: string[];
}

/** 拖到某个面板上时，面板提供的目标信息 */
export interface DropTarget {
  paneId: string;
  loc: Loc;
  home: string | null;
  dir: string;
  title: string;
}

/** os：从资源管理器拖进来的（系统自己画拖动的图标，这边不画跟随鼠标的提示） */
interface DragState { payload: DragPayload; x: number; y: number; over: string | null; os?: boolean }

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

/** 把拖着的文件放到 (x, y) 处的面板上（那里不能放就什么也不做） */
function dropAt(payload: DragPayload, x: number, y: number): void {
  const over = paneAt(x, y);
  const target = dropTargetInfo(over, payload);
  if (over && target) targets.get(over)?.drop(payload, target);
}

/** 指针是否在窗口外（坐标是相对窗口的 CSS 像素） */
export function outsideWindow(x: number, y: number, width: number, height: number): boolean {
  return x < 0 || y < 0 || x >= width || y >= height;
}

/**
 * 拖出窗口：交给系统拖放（服务器上的文件拖到资源管理器等）。
 * @returns 又拖回本窗口松手时，松手的位置（CSS 像素），按窗口内的拖放处理；其他情况为 null
 */
export type DragOut = (payload: DragPayload) => Promise<{ x: number; y: number } | null>;

/** 正在拖出窗口的文件（系统拖放进行中） */
let outPayload: DragPayload | null = null;

/** 拖出窗口：等系统拖放结束；拖回本窗口松手的，按窗口内的拖放放下 */
async function dragOutOf(payload: DragPayload, out: DragOut): Promise<void> {
  outPayload = payload;
  try {
    const back = await out(payload);
    if (back) dropAt(payload, back.x, back.y);
  } finally {
    outPayload = null;
    if (state?.os) { state = null; emit(); }
  }
}

/**
 * 在文件行上按下鼠标后调用：移动超过几个像素才算开始拖动（否则是普通点击）。
 * Esc 取消拖动。
 * @param out 拖出窗口时调用（服务器文件栏在 Windows 上传）：结束窗口内的拖动，交给系统拖放。
 *   WebView2 在按住鼠标时窗口外也会给 pointermove；收不到的话只是拖不出去
 */
export function beginDrag(e: ReactPointerEvent, payload: () => DragPayload | null, out?: DragOut): void {
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
    if (out && state.payload.loc.kind !== "local" && outsideWindow(ev.clientX, ev.clientY, window.innerWidth, window.innerHeight)) {
      const p = state.payload;
      cleanup();
      void dragOutOf(p, out);
      return;
    }
    state = { ...state, x: ev.clientX, y: ev.clientY, over: paneAt(ev.clientX, ev.clientY) };
    emit();
  };
  const up = (ev: PointerEvent) => {
    const s = state;
    cleanup();
    if (!started || !s) return;
    dropAt(s.payload, ev.clientX, ev.clientY);
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

// ---------- 从资源管理器拖进来的文件（系统拖放） ----------
// 窗口接管了系统拖放（网页收不到外部文件的 HTML5 drop），由终端区收到 Tauri 的拖放事件后转到这里。
// 落在文件栏上就交给文件栏，和应用内拖动走同一条路（同名确认、传输列表）；落在别处返回 false，由调用方处理

let osPaths: string[] = [];

/** 拖进来的文件当作来自本机的一次拖动 */
function systemPayload(paths: string[]): DragPayload | null {
  if (!paths.length) return null;
  return { paneId: "os", loc: { kind: "local" }, home: null, dir: parentPath(paths[0]) ?? paths[0], names: paths.map(baseName), paths };
}

/**
 * 从资源管理器拖着文件经过窗口。
 * @param paths 拖着的文件（只有刚进窗口那一下带；之后为 null，用记下的）
 * @param x 鼠标位置（CSS 像素，相对窗口）
 * @param y 同上
 * @returns 是否在文件栏上（是的话由文件栏高亮，调用方不用再管）；从服务器文件栏拖出窗口又拖回来的，
 *   一律由文件栏处理（返回 true）
 */
export function systemDragOver(paths: string[] | null, x: number, y: number): boolean {
  // 拖出窗口后又拖回来：按原来的文件高亮能放的面板，松手后由拖出那边按窗口内的拖放处理
  if (outPayload) {
    state = { payload: outPayload, x, y, over: paneAt(x, y), os: true };
    emit();
    return true;
  }
  if (paths) osPaths = paths;
  const over = paneAt(x, y);
  const payload = systemPayload(osPaths);
  if (!over || !payload) {
    if (state?.os) { state = null; emit(); }
    return false;
  }
  state = { payload, x, y, over, os: true };
  emit();
  return true;
}

/** 拖动离开窗口或结束：去掉高亮 */
export function systemDragEnd(): void {
  osPaths = [];
  if (state?.os) { state = null; emit(); }
}

/**
 * 从资源管理器拖进来的文件放下了。
 * @returns 是否落在文件栏上（是的话已经交给文件栏，没打开目录时什么也不做）
 */
export function systemDrop(paths: string[], x: number, y: number): boolean {
  systemDragEnd();
  // 拖出窗口的文件拖回来松手时，拖出那边会取消放下，正常走不到这里；万一放下了，也按原来的文件处理，
  // 不当作从资源管理器拖进来的临时文件
  if (outPayload) {
    dropAt(outPayload, x, y);
    return true;
  }
  const over = paneAt(x, y);
  const payload = systemPayload(paths);
  if (!over) return false;
  const target = payload ? dropTargetInfo(over, payload) : null;
  if (payload && target) targets.get(over)?.drop(payload, target);
  return true;
}
