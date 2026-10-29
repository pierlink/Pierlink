import { describe, expect, it } from "vitest";
import { DEFAULT_SETTINGS, normalizeSettings } from "./model";

describe("normalizeSettings", () => {
  it("以前开过“右键直接复制”或“右键直接粘贴”，升级后当作打开了“右键复制粘贴”", () => {
    expect(normalizeSettings({ terminal: { rightClickPaste: true } }).terminal.rightClickCopyPaste).toBe(true);
    expect(normalizeSettings({ terminal: { rightClickCopy: true, rightClickPaste: false } }).terminal.rightClickCopyPaste).toBe(true);
    expect(normalizeSettings({ terminal: { rightClickCopy: false, rightClickPaste: false } }).terminal.rightClickCopyPaste).toBe(false);
    // 新开关保存过就以它为准
    expect(normalizeSettings({ terminal: { rightClickCopyPaste: false, rightClickPaste: true } }).terminal.rightClickCopyPaste).toBe(false);
    // 去掉的旧设置不再出现
    expect(normalizeSettings({ terminal: { pasteKey: "ctrlShiftV" } }).terminal).not.toHaveProperty("pasteKey");
  });
  it("空数据得到默认设置", () => {
    expect(normalizeSettings(undefined)).toEqual(DEFAULT_SETTINGS);
    expect(normalizeSettings("乱码")).toEqual(DEFAULT_SETTINGS);
  });
  it("数值收回范围、未知取值改回默认", () => {
    const s = normalizeSettings({ theme: "pink", terminal: { fontSize: 99, scrollback: 5, cursorStyle: "x" }, syncDefaults: { watchDelayS: 0 } });
    expect(s.theme).toBe("dark");
    expect(s.terminal.fontSize).toBe(24);
    expect(s.terminal.scrollback).toBe(1000);
    expect(s.terminal.cursorStyle).toBe("bar");
    expect(s.syncDefaults.watchDelayS).toBe(1);
  });
  it("保留合法的值，排除规则去掉空行", () => {
    const s = normalizeSettings({ theme: "light", language: "en", syncDefaults: { ignoreRules: [" a ", "", "b"] } });
    expect(s.theme).toBe("light");
    expect(s.language).toBe("en");
    expect(s.syncDefaults.ignoreRules).toEqual(["a", "b"]);
  });
});
