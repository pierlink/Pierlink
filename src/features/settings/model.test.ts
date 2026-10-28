import { describe, expect, it } from "vitest";
import { DEFAULT_SETTINGS, normalizeSettings } from "./model";

describe("normalizeSettings", () => {
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
