import { describe, expect, it } from "vitest";
import i18n from "./i18n";
import { formatError, isAppError } from "./errors";

describe("错误翻译", () => {
  it("有翻译时代入参数", async () => {
    await i18n.changeLanguage("zh-CN");
    expect(formatError({ code: "ssh.invalid_argument", params: { field: "port" } })).toBe("连接参数无效：port");
    await i18n.changeLanguage("en");
    expect(formatError({ code: "ssh.invalid_argument", params: { field: "port" } })).toBe("Invalid connection setting: port");
  });
  it("内置 SSH 的固定说明按当前语言显示，不混进后端的中文兜底", async () => {
    const err = { code: "ssh.unreachable", params: { detail: "disconnected_login", reason: "中文兜底", stderr_tail: "中文兜底", engine: "builtin" } };
    await i18n.changeLanguage("en");
    const text = formatError(err);
    expect(text).toContain("dropped while signing in");
    expect(text).not.toMatch(/[\u4e00-\u9fff]/);
    await i18n.changeLanguage("zh-CN");
    expect(formatError(err)).toContain("登录过程中连接断开了");
    // 没有对应翻译的 detail：照旧显示后端给的文字
    expect(formatError({ code: "ssh.failed", params: { detail: "未知", stderr_tail: "ssh 原文" } })).toBe("SSH 连接失败（ssh 原文）");
  });
  it("没有翻译时显示错误码", async () => {
    await i18n.changeLanguage("zh-CN");
    expect(formatError({ code: "x.y", params: {} })).toBe("出了点问题（错误码 x.y）");
  });
  it("不是后端错误时给通用提示", () => {
    expect(isAppError(new Error("a"))).toBe(false);
    expect(formatError("字符串")).toContain("frontend");
  });
});
