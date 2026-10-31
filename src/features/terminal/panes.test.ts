import { describe, expect, it } from "vitest";
import { computeRects, insertBeside, pairNodes, pairSpan, pane, paneIds, removePane, resizeAt, seriesCount, splitPane, swapPanes } from "./panes";

describe("分屏布局", () => {
  it("左右拆分后各占一半，再同方向拆分平分原来那一格", () => {
    let l = splitPane(pane("a"), "a", "row", "b");
    expect(paneIds(l)).toEqual(["a", "b"]);
    let r = computeRects(l);
    expect(r.panes.a).toEqual({ x: 0, y: 0, w: 50, h: 100 });
    expect(r.panes.b).toEqual({ x: 50, y: 0, w: 50, h: 100 });
    expect(r.dividers).toHaveLength(1);
    l = splitPane(l, "b", "row", "c");
    expect(paneIds(l)).toEqual(["a", "b", "c"]);
    r = computeRects(l);
    expect(r.panes.c.x).toBeCloseTo(75);
    expect(r.panes.c.w).toBeCloseTo(25);
  });
  it("不同方向拆分会嵌套，位置正确", () => {
    const l = splitPane(splitPane(pane("a"), "a", "row", "b"), "b", "col", "c");
    const r = computeRects(l);
    expect(r.panes.b).toEqual({ x: 50, y: 0, w: 50, h: 50 });
    expect(r.panes.c).toEqual({ x: 50, y: 50, w: 50, h: 50 });
    expect(r.dividers.map((d) => d.dir).sort()).toEqual(["col", "row"]);
  });
  it("关闭窗格把空间还给相邻的，只剩一个时合并，最后一个返回 null", () => {
    const l = splitPane(splitPane(pane("a"), "a", "row", "b"), "b", "col", "c");
    const l2 = removePane(l, "c")!;
    expect(computeRects(l2).panes.b).toEqual({ x: 50, y: 0, w: 50, h: 100 });
    const l3 = removePane(l2, "a")!;
    expect(l3).toEqual(pane("b"));
    expect(removePane(l3, "b")).toBeNull();
  });
  it("拖动分隔条按比例调整并限制最小值", () => {
    const l = splitPane(pane("a"), "a", "row", "b");
    expect(computeRects(resizeAt(l, [], 0, 0.3)).panes.a.w).toBeCloseTo(30);
    expect(computeRects(resizeAt(l, [], 0, 0.01)).panes.a.w).toBeCloseTo(10);
    expect(computeRects(resizeAt(l, [], 0, 0.99)).panes.b.w).toBeCloseTo(10);
    const nested = splitPane(l, "b", "col", "c");
    const span = pairSpan(nested, [1], 0)!;
    expect(span).toEqual({ start: 0, end: 100, dir: "col" });
    expect(computeRects(resizeAt(nested, [1], 0, 0.25)).panes.c.h).toBeCloseTo(75);
  });
});

describe("嵌套窗格的最小尺寸", () => {
  // a | (b | c) 左右，右边再左右分：右边子树横向串联 2 个窗格
  const l = splitPane(splitPane(pane("a"), "a", "row", "b"), "b", "row", "c");
  const nested = splitPane(pane("a"), "a", "row", "b");
  const deep = splitPane(splitPane(nested, "b", "col", "c"), "c", "row", "d");
  it("串联数：同方向相加，另一方向取最大", () => {
    expect(seriesCount(l, "row")).toBe(3);
    expect(seriesCount(deep, "row")).toBe(3);
    expect(seriesCount(deep, "col")).toBe(2);
  });
  it("两边最小比例不同", () => {
    const [left, right] = pairNodes(deep, [], 0)!;
    expect(seriesCount(left, "row")).toBe(1);
    expect(seriesCount(right, "row")).toBe(2);
    // 前一格至少 10%，后一格至少 40%：拖到 90% 处被限制在 60%
    expect(computeRects(resizeAt(deep, [], 0, 0.9, 0.1, 0.4)).panes.a.w).toBeCloseTo(60);
  });
  it("两边最小值加起来超过 1 时按比例缩小", () => {
    const r = computeRects(resizeAt(nested, [], 0, 0.1, 0.8, 0.8)).panes;
    expect(r.a.w).toBeCloseTo(50);
  });
});

describe("拖放调整窗格", () => {
  it("放到另一格的左边、下边，目标那一格对半分", () => {
    const l = splitPane(pane("a"), "a", "row", "b");
    const left = insertBeside(l, "b", "left", pane("c"));
    expect(paneIds(left)).toEqual(["a", "c", "b"]);
    const r = computeRects(left);
    expect(r.panes.a.w).toBeCloseTo(50);
    expect(r.panes.c.w).toBeCloseTo(25);
    expect(r.panes.b.x).toBeCloseTo(75);
    const down = insertBeside(l, "a", "bottom", pane("c"));
    const r2 = computeRects(down);
    expect(r2.panes.c).toEqual({ x: 0, y: 50, w: 50, h: 50 });
    expect(r2.panes.b.w).toBeCloseTo(50);
  });
  it("单个窗格的布局也能放，插入同方向的整组时展开、比例保持", () => {
    expect(paneIds(insertBeside(pane("a"), "a", "top", pane("b")))).toEqual(["b", "a"]);
    const group = splitPane(pane("x"), "x", "row", "y");
    const l = insertBeside(pane("a"), "a", "right", group);
    expect(l.kind === "split" && l.children.length).toBe(3);
    const r = computeRects(l);
    expect(r.panes.a.w).toBeCloseTo(50);
    expect(r.panes.x.w).toBeCloseTo(25);
    expect(r.panes.y.w).toBeCloseTo(25);
  });
  it("移动 = 先去掉再插入；交换两个窗格", () => {
    const l = splitPane(splitPane(pane("a"), "a", "row", "b"), "b", "row", "c");
    const moved = insertBeside(removePane(l, "a")!, "c", "right", pane("a"));
    expect(paneIds(moved)).toEqual(["b", "c", "a"]);
    expect(paneIds(swapPanes(l, "a", "c"))).toEqual(["c", "b", "a"]);
  });
  it("找不到目标时原样返回", () => {
    const l = pane("a");
    expect(insertBeside(l, "zz", "left", pane("b"))).toBe(l);
  });
});
