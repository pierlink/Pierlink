/**
 * 分屏布局：一个标签里的窗格排成一棵树。叶子是窗格，中间节点是“左右”或“上下”拆分，
 * 每个子项占的比例存在 sizes 里（和为 1）。
 *
 * 渲染时不按树嵌套组件，而是把每个窗格算成一个矩形（占容器的百分比）绝对定位：
 * 这样拆分、关闭、拖动分隔条时，已有窗格的组件不会被重建（重建会断开终端会话）。
 */

export type Dir = "row" | "col";
export type Layout =
  | { kind: "pane"; id: string }
  | { kind: "split"; dir: Dir; children: Layout[]; sizes: number[] };

/** 百分比矩形（0–100） */
export interface Rect { x: number; y: number; w: number; h: number }

/** 一条分隔条：在哪个拆分节点（从根开始的子项下标路径）、第几个间隔，以及它的位置 */
export interface Divider { path: number[]; index: number; dir: Dir; rect: Rect }

export function pane(id: string): Layout {
  return { kind: "pane", id };
}

/** 所有窗格 ID（从左到右、从上到下） */
export function paneIds(layout: Layout): string[] {
  return layout.kind === "pane" ? [layout.id] : layout.children.flatMap(paneIds);
}

/**
 * 在某个窗格旁边拆出一个新窗格。
 * @param dir row 为左右（新窗格在右边），col 为上下（新窗格在下边）
 * @returns 新布局；找不到目标窗格时原样返回
 */
export function splitPane(layout: Layout, target: string, dir: Dir, newId: string): Layout {
  if (layout.kind === "pane") {
    if (layout.id !== target) return layout;
    return { kind: "split", dir, children: [layout, pane(newId)], sizes: [0.5, 0.5] };
  }
  const i = layout.children.findIndex((c) => c.kind === "pane" && c.id === target);
  if (i >= 0 && layout.dir === dir) {
    // 同方向的拆分直接插在后面，把原来那一格平分
    const children = [...layout.children];
    const sizes = [...layout.sizes];
    const half = sizes[i] / 2;
    children.splice(i + 1, 0, pane(newId));
    sizes.splice(i, 1, half, half);
    return { ...layout, children, sizes };
  }
  return { ...layout, children: layout.children.map((c) => splitPane(c, target, dir, newId)) };
}

/**
 * 去掉一个窗格，它占的空间分给相邻的窗格；只剩一个子项的拆分会被合并掉。
 * @returns 新布局；去掉的是最后一个窗格时返回 null
 */
export function removePane(layout: Layout, target: string): Layout | null {
  if (layout.kind === "pane") return layout.id === target ? null : layout;
  const children: Layout[] = [];
  const sizes: number[] = [];
  layout.children.forEach((c, i) => {
    const next = removePane(c, target);
    if (next) { children.push(next); sizes.push(layout.sizes[i]); }
  });
  if (children.length === 0) return null;
  if (children.length === 1) return children[0];
  const sum = sizes.reduce((a, b) => a + b, 0);
  return { ...layout, children, sizes: sizes.map((s) => s / sum) };
}

/**
 * 算出每个窗格和每条分隔条的位置。
 * @param gap 窗格之间的间隔（百分比单位里的近似值，只用于分隔条的可点击宽度定位）
 */
export function computeRects(layout: Layout, area: Rect = { x: 0, y: 0, w: 100, h: 100 }, path: number[] = []): { panes: Record<string, Rect>; dividers: Divider[] } {
  if (layout.kind === "pane") return { panes: { [layout.id]: area }, dividers: [] };
  const panes: Record<string, Rect> = {};
  const dividers: Divider[] = [];
  let offset = 0;
  layout.children.forEach((child, i) => {
    const size = layout.sizes[i];
    const rect: Rect = layout.dir === "row"
      ? { x: area.x + area.w * offset, y: area.y, w: area.w * size, h: area.h }
      : { x: area.x, y: area.y + area.h * offset, w: area.w, h: area.h * size };
    const inner = computeRects(child, rect, [...path, i]);
    Object.assign(panes, inner.panes);
    dividers.push(...inner.dividers);
    offset += size;
    if (i < layout.children.length - 1) {
      dividers.push({
        path,
        index: i,
        dir: layout.dir,
        rect: layout.dir === "row"
          ? { x: area.x + area.w * offset, y: area.y, w: 0, h: area.h }
          : { x: area.x, y: area.y + area.h * offset, w: area.w, h: 0 },
      });
    }
  });
  return { panes, dividers };
}

/** 找到 path 指向的拆分节点 */
function nodeAt(layout: Layout, path: number[]): Layout | null {
  let node: Layout = layout;
  for (const i of path) {
    if (node.kind !== "split" || !node.children[i]) return null;
    node = node.children[i];
  }
  return node;
}

/**
 * 某个子树在 dir 方向上最少要容纳几个窗格（串联的窗格数）：
 * 同方向拆分是各子项之和，另一方向拆分取各子项的最大值。拖动分隔条时据此算最小尺寸，嵌套的窗格也不会被挤没。
 */
export function seriesCount(layout: Layout, dir: Dir): number {
  if (layout.kind === "pane") return 1;
  const counts = layout.children.map((c) => seriesCount(c, dir));
  return layout.dir === dir ? counts.reduce((a, b) => a + b, 0) : Math.max(...counts);
}

/**
 * 拖动分隔条：把第 index 和 index+1 两格之间的边界移动到 fraction（相对这两格合起来的范围，0–1）。
 * @param minA 前一格至少占这两格合计的比例
 * @param minB 后一格至少占的比例（两者之和超过 1 时按比例缩小，保证仍能拖动）
 */
export function resizeAt(layout: Layout, path: number[], index: number, fraction: number, minA = 0.1, minB = minA): Layout {
  const node = nodeAt(layout, path);
  if (!node || node.kind !== "split" || index < 0 || index >= node.children.length - 1) return layout;
  const pair = node.sizes[index] + node.sizes[index + 1];
  const scale = minA + minB > 1 ? 1 / (minA + minB) : 1;
  const lo = minA * scale;
  const hi = 1 - minB * scale;
  const f = Math.min(hi, Math.max(lo, fraction));
  const sizes = [...node.sizes];
  sizes[index] = pair * f;
  sizes[index + 1] = pair * (1 - f);
  const replaced: Layout = { ...node, sizes };
  const rebuild = (n: Layout, depth: number): Layout => {
    if (depth === path.length) return replaced;
    if (n.kind !== "split") return n;
    return { ...n, children: n.children.map((c, i) => (i === path[depth] ? rebuild(c, depth + 1) : c)) };
  };
  return rebuild(layout, 0);
}

/** 拆分节点里相邻两格的子树（拖动时算各自的最小尺寸） */
export function pairNodes(layout: Layout, path: number[], index: number): [Layout, Layout] | null {
  const node = nodeAt(layout, path);
  if (!node || node.kind !== "split" || !node.children[index + 1]) return null;
  return [node.children[index], node.children[index + 1]];
}

/** 把两格之间的边界在 path 节点里的起止位置（百分比）算出来，供拖动时换算 */
export function pairSpan(layout: Layout, path: number[], index: number): { start: number; end: number; dir: Dir } | null {
  // 先算出该节点的矩形
  let area: Rect = { x: 0, y: 0, w: 100, h: 100 };
  let node: Layout = layout;
  for (const i of path) {
    if (node.kind !== "split") return null;
    const before = node.sizes.slice(0, i).reduce((a, b) => a + b, 0);
    area = node.dir === "row"
      ? { x: area.x + area.w * before, y: area.y, w: area.w * node.sizes[i], h: area.h }
      : { x: area.x, y: area.y + area.h * before, w: area.w, h: area.h * node.sizes[i] };
    node = node.children[i];
  }
  if (node.kind !== "split") return null;
  const before = node.sizes.slice(0, index).reduce((a, b) => a + b, 0);
  const pair = node.sizes[index] + node.sizes[index + 1];
  if (node.dir === "row") return { start: area.x + area.w * before, end: area.x + area.w * (before + pair), dir: "row" };
  return { start: area.y + area.h * before, end: area.y + area.h * (before + pair), dir: "col" };
}

/** 拖放窗格时放到目标窗格的哪一边 */
export type Side = "left" | "right" | "top" | "bottom";

/**
 * 把 node（一个窗格或一整个标签的布局）放到 target 窗格的某一边，target 原来那一格对半分。
 * node 本身是同方向的拆分时展开插入（不多套一层）。
 * @returns 新布局；找不到 target 时原样返回
 */
export function insertBeside(layout: Layout, target: string, side: Side, node: Layout): Layout {
  const dir: Dir = side === "left" || side === "right" ? "row" : "col";
  const before = side === "left" || side === "top";
  /** 要插入的若干子项及各自在“半格”里占的比例 */
  const parts: { children: Layout[]; sizes: number[] } = node.kind === "split" && node.dir === dir
    ? { children: node.children, sizes: node.sizes }
    : { children: [node], sizes: [1] };
  const wrap = (leaf: Layout): Layout => ({
    kind: "split",
    dir,
    children: before ? [...parts.children, leaf] : [leaf, ...parts.children],
    sizes: before ? [...parts.sizes.map((s) => s / 2), 0.5] : [0.5, ...parts.sizes.map((s) => s / 2)],
  });
  const walk = (n: Layout): Layout => {
    if (n.kind === "pane") return n.id === target ? wrap(n) : n;
    const i = n.children.findIndex((c) => c.kind === "pane" && c.id === target);
    if (i >= 0 && n.dir === dir) {
      const half = n.sizes[i] / 2;
      const children = [...n.children];
      const sizes = [...n.sizes];
      const added = parts.sizes.map((s) => s * half);
      children.splice(before ? i : i + 1, 0, ...parts.children);
      sizes.splice(i, 1, ...(before ? [...added, half] : [half, ...added]));
      return { ...n, children, sizes };
    }
    return { ...n, children: n.children.map(walk) };
  };
  return walk(layout);
}

/** 交换两个窗格的位置（同一个布局里） */
export function swapPanes(layout: Layout, a: string, b: string): Layout {
  if (layout.kind === "pane") return layout.id === a ? pane(b) : layout.id === b ? pane(a) : layout;
  return { ...layout, children: layout.children.map((c) => swapPanes(c, a, b)) };
}
