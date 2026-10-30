import type { Side } from "./panes";

/** 拖动的是什么：一个窗格（拖标题栏），或一整个标签（拖标签） */
export type DragWhat = { kind: "pane"; id: string; tab: string } | { kind: "tab"; tab: string };

/** 放到哪里 */
export type DropAt =
  /** 放到某个窗格的一边；swap 为放在中间，与它交换位置（只限同一个标签里拖窗格） */
  | { kind: "pane"; tab: string; pane: string; side: Side | "swap" }
  /** 放到标签上：前面/后面（调整标签顺序，只限拖标签），或合并进这个标签 */
  | { kind: "tab"; tab: string; part: "before" | "after" | "merge" }
  /** 放到标签栏空白处：窗格单独成为一个新标签 */
  | { kind: "newtab" };

/** 屏幕上的矩形（像素） */
export interface Box { left: number; top: number; width: number; height: number }

/** 窗格中间这么大的区域算“交换位置”（相对宽高的比例，离四边都超过它） */
const CENTER = 0.3;

/**
 * 指针在窗格里的位置 → 放到哪一边。离哪条边最近就是哪边；在中间区域时交换（allowSwap 为 false 时仍取最近的边）。
 * @param fx 指针相对窗格左边的比例（0–1）
 * @param fy 指针相对窗格上边的比例（0–1）
 */
export function zoneAt(fx: number, fy: number, allowSwap: boolean): Side | "swap" {
  const d: [Side, number][] = [["left", fx], ["right", 1 - fx], ["top", fy], ["bottom", 1 - fy]];
  d.sort((a, b) => a[1] - b[1]);
  if (allowSwap && d[0][1] > CENTER) return "swap";
  return d[0][0];
}

/** 放下后高亮的区域：一边是窗格的那一半，交换是整个窗格 */
export function zoneBox(r: Box, side: Side | "swap"): Box {
  switch (side) {
    case "left": return { ...r, width: r.width / 2 };
    case "right": return { ...r, left: r.left + r.width / 2, width: r.width / 2 };
    case "top": return { ...r, height: r.height / 2 };
    case "bottom": return { ...r, top: r.top + r.height / 2, height: r.height / 2 };
    default: return r;
  }
}

/**
 * 指针在标签上的位置 → 放到前面、后面，还是合并进去。
 * 拖窗格时只有合并；拖标签时左右各四分之一是调整顺序，中间是合并。
 */
export function tabPart(fx: number, draggingTab: boolean): "before" | "after" | "merge" {
  if (!draggingTab) return "merge";
  if (fx < 0.25) return "before";
  if (fx > 0.75) return "after";
  return "merge";
}

/** 调整标签顺序：把 key 移到 target 的前面或后面 */
export function reorder<T>(list: T[], keyOf: (x: T) => string, key: string, target: string, part: "before" | "after"): T[] {
  const item = list.find((x) => keyOf(x) === key);
  if (!item || key === target) return list;
  const rest = list.filter((x) => keyOf(x) !== key);
  const i = rest.findIndex((x) => keyOf(x) === target);
  if (i < 0) return list;
  rest.splice(part === "before" ? i : i + 1, 0, item);
  return rest;
}
