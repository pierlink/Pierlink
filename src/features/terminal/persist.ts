import { paneIds, type Layout } from "./panes";
import type { OpenSpec } from "./types";

/**
 * 应用重启后恢复上次的标签布局（设计：标签显示“未连接”，按 Enter 或点击重连）。
 * 只保存布局和每个窗格打开的是什么终端，不保存终端内容；存在本机浏览器存储里（和设置一样）。
 */

const KEY = "pierlink.workspace.v1";

/** 保存的一个窗格 */
export interface SavedPane {
  spec: OpenSpec;
  title: string;
  subtitle: string;
  files: { open: boolean; width: number };
  /** 服务器桌面窗格（不是终端） */
  desktop?: boolean;
}

/** 保存的一个标签 */
export interface SavedTab {
  layout: Layout;
  active: string;
  panes: Record<string, SavedPane>;
}

export interface SavedWorkspace {
  tabs: SavedTab[];
  /** 当前标签的序号 */
  active: number;
}

function isSpec(v: unknown): v is OpenSpec {
  const s = v as Record<string, unknown> | null;
  if (!s || typeof s !== "object") return false;
  if (s.kind === "local") return typeof s.shellId === "string";
  if (s.kind === "server") return typeof s.serverId === "string";
  if (s.kind === "quick") return typeof s.host === "string" && typeof s.port === "number" && (s.user === null || typeof s.user === "string");
  return false;
}

function isLayout(v: unknown, depth = 0): v is Layout {
  const l = v as Record<string, unknown> | null;
  if (!l || typeof l !== "object" || depth > 32) return false;
  if (l.kind === "pane") return typeof l.id === "string";
  if (l.kind !== "split" || (l.dir !== "row" && l.dir !== "col")) return false;
  const children = l.children as unknown[];
  const sizes = l.sizes as unknown[];
  return Array.isArray(children) && Array.isArray(sizes) && children.length >= 2 && children.length === sizes.length
    && sizes.every((x) => typeof x === "number" && x > 0 && x <= 1)
    && children.every((c) => isLayout(c, depth + 1));
}

/**
 * 检查并整理读到的数据：格式不对的标签整个丢掉，布局里引用的窗格必须都有记录。
 * @returns 可用的工作区；没有可恢复的内容时为 null
 */
export function parseSaved(raw: unknown): SavedWorkspace | null {
  const w = raw as { tabs?: unknown; active?: unknown } | null;
  if (!w || !Array.isArray(w.tabs)) return null;
  const tabs: SavedTab[] = [];
  for (const t of w.tabs as Record<string, unknown>[]) {
    if (!t || !isLayout(t.layout) || typeof t.panes !== "object" || !t.panes) continue;
    const ids = paneIds(t.layout);
    // 窗格 ID 不能重复，否则恢复后两个窗格共用一个组件
    if (new Set(ids).size !== ids.length) continue;
    const panes = t.panes as Record<string, Record<string, unknown>>;
    const ok = ids.every((id) => {
      const p = panes[id];
      return p && isSpec(p.spec) && typeof p.title === "string" && typeof p.subtitle === "string";
    });
    if (!ok) continue;
    const clean: Record<string, SavedPane> = {};
    for (const id of ids) {
      const p = panes[id];
      const f = p.files as { open?: unknown; width?: unknown } | undefined;
      clean[id] = {
        spec: p.spec as OpenSpec,
        title: p.title as string,
        subtitle: p.subtitle as string,
        files: { open: f?.open === true, width: typeof f?.width === "number" && f.width > 0 ? f.width : 300 },
        ...(p.desktop === true && (p.spec as OpenSpec).kind === "server" ? { desktop: true } : {}),
      };
    }
    tabs.push({ layout: t.layout, active: typeof t.active === "string" && ids.includes(t.active) ? t.active : ids[0], panes: clean });
  }
  if (!tabs.length) return null;
  const active = typeof w.active === "number" && w.active >= 0 && w.active < tabs.length ? Math.floor(w.active) : 0;
  return { tabs, active };
}

/** 读取上次保存的工作区（没有或读取失败时为 null） */
export function loadWorkspace(): SavedWorkspace | null {
  try {
    const text = window.localStorage.getItem(KEY);
    return text ? parseSaved(JSON.parse(text)) : null;
  } catch {
    return null;
  }
}

/** 保存工作区；没有标签时清除 */
export function saveWorkspace(w: SavedWorkspace): void {
  try {
    if (w.tabs.length) window.localStorage.setItem(KEY, JSON.stringify(w));
    else window.localStorage.removeItem(KEY);
  } catch {
    // 存储不可用（隐私模式等）时只是不能恢复，不影响使用
  }
}

/** 恢复时窗格 ID 计数要从已有最大编号之后开始，避免新窗格和恢复的窗格同名 */
export function maxPaneNumber(w: SavedWorkspace): number {
  let max = 0;
  for (const t of w.tabs) {
    for (const id of paneIds(t.layout)) {
      const n = Number(/^pane-(\d+)$/.exec(id)?.[1] ?? 0);
      if (n > max) max = n;
    }
  }
  return max;
}
