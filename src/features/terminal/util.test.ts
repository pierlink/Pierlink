import { describe, expect, it } from "vitest";
import { base64ToBytes, needsPasteConfirm, parseQuickTarget, parseOsc7, parseOsc99 } from "./util";

describe("快速连接解析", () => {
  it("常见写法", () => {
    expect(parseQuickTarget("dev@203.0.113.18")).toEqual({ kind: "quick", host: "203.0.113.18", port: 22, user: "dev" });
    expect(parseQuickTarget("example.com:2222")).toEqual({ kind: "quick", host: "example.com", port: 2222, user: null });
    expect(parseQuickTarget("u@[::1]:2200")).toEqual({ kind: "quick", host: "::1", port: 2200, user: "u" });
    expect(parseQuickTarget("user@corp.local@host")).toEqual({ kind: "quick", host: "host", port: 22, user: "user@corp.local" });
  });
  it("拒绝无效输入", () => {
    for (const bad of ["", "a b", "-oProxyCommand=x", "h:0", "h:70000", "h:x", "::1", "u@"]) {
      expect(parseQuickTarget(bad)).toBeNull();
    }
  });
});

describe("粘贴确认", () => {
  it("多行且未开括号粘贴时确认", () => {
    expect(needsPasteConfirm("ls\nrm -rf x", false)).toBe(true);
    expect(needsPasteConfirm("ls\nrm -rf x", true)).toBe(false);
    expect(needsPasteConfirm("ls\n", false)).toBe(false);
    expect(needsPasteConfirm("单行", false)).toBe(false);
    expect(needsPasteConfirm("a\nb", true, true)).toBe(true);
  });
});

describe("base64", () => {
  it("中文按字节还原", () => {
    const bytes = base64ToBytes(btoa(String.fromCharCode(...new TextEncoder().encode("中文"))));
    expect(new TextDecoder().decode(bytes)).toBe("中文");
  });
});

describe("当前目录报告", () => {
  it("OSC 7：Linux 路径解码", () => {
    expect(parseOsc7("file://dev-box/home/me/my%20dir")).toBe("/home/me/my dir");
    expect(parseOsc7("file:///tmp")).toBe("/tmp");
  });
  it("OSC 7：Windows 盘符路径", () => {
    expect(parseOsc7("file://PC/C:/Users/me")).toBe("C:\\Users\\me");
    expect(parseOsc7("file://PC/D:")).toBe("D:");
  });
  it("OSC 7：格式不对返回 null", () => {
    expect(parseOsc7("http://x/y")).toBeNull();
    expect(parseOsc7("file://h/%E0%A4%A")).toBeNull();
  });
  it("OSC 9;9：有无引号都可以", () => {
    expect(parseOsc99('"C:\\Users\\me"')).toBe("C:\\Users\\me");
    expect(parseOsc99("C:\\x")).toBe("C:\\x");
    expect(parseOsc99('""')).toBeNull();
  });
});
