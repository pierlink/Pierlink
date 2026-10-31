import { describe, expect, it } from "vitest";
import { maxPaneNumber, parseSaved } from "./persist";
import { pane, splitPane } from "./panes";

const local = { spec: { kind: "local", shellId: "pwsh" }, title: "PowerShell 7", subtitle: "本机", files: { open: true, width: 320 } };
const server = { spec: { kind: "server", serverId: "s1" }, title: "gpu", subtitle: "dev@h", files: { open: false, width: 300 } };

describe("工作区恢复数据", () => {
  it("正常数据原样恢复", () => {
    const layout = splitPane(pane("pane-1"), "pane-1", "row", "pane-7");
    const w = parseSaved({ tabs: [{ layout, active: "pane-7", panes: { "pane-1": local, "pane-7": server } }], active: 0 });
    expect(w?.tabs[0].panes["pane-7"].spec).toEqual({ kind: "server", serverId: "s1" });
    expect(w?.tabs[0].active).toBe("pane-7");
    expect(maxPaneNumber(w!)).toBe(7);
  });
  it("坏数据整个标签丢掉，其余保留", () => {
    const good = { layout: pane("pane-1"), active: "pane-1", panes: { "pane-1": local } };
    const missing = { layout: splitPane(pane("pane-2"), "pane-2", "col", "pane-3"), active: "pane-2", panes: { "pane-2": local } };
    const badSpec = { layout: pane("pane-4"), active: "pane-4", panes: { "pane-4": { ...local, spec: { kind: "evil" } } } };
    const dup = { layout: { kind: "split", dir: "row", children: [pane("pane-5"), pane("pane-5")], sizes: [0.5, 0.5] }, active: "pane-5", panes: { "pane-5": local } };
    const w = parseSaved({ tabs: [missing, good, badSpec, dup], active: 9 });
    expect(w?.tabs.length).toBe(1);
    expect(w?.active).toBe(0);
  });
  it("完全不对的数据返回 null", () => {
    expect(parseSaved(null)).toBeNull();
    expect(parseSaved({ tabs: "x" })).toBeNull();
    expect(parseSaved({ tabs: [] })).toBeNull();
    expect(parseSaved({ tabs: [{ layout: { kind: "split", dir: "row", children: [], sizes: [] }, panes: {} }] })).toBeNull();
  });
  it("缺少的字段用默认值，当前窗格不存在时用第一个", () => {
    const w = parseSaved({ tabs: [{ layout: pane("pane-1"), active: "nope", panes: { "pane-1": { spec: local.spec, title: "a", subtitle: "b" } } }] });
    expect(w?.tabs[0].panes["pane-1"].files).toEqual({ open: false, width: 300 });
    expect(w?.tabs[0].active).toBe("pane-1");
  });
});
