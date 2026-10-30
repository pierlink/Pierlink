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
  it("后端的固定说明按当前语言显示，不混进后端的中文原文", async () => {
    const err = {
      code: "ssh.unreachable",
      params: { reason: "中文原文", reason_key: "disconnected_login", stderr_tail: "中文原文", stderr_tail_key: "disconnected_login", engine: "builtin" },
    };
    await i18n.changeLanguage("en");
    const text = formatError(err);
    expect(text).toContain("dropped while signing in");
    expect(text).not.toMatch(/[\u4e00-\u9fff]/);
    await i18n.changeLanguage("zh-CN");
    expect(formatError(err)).toContain("登录过程中连接断开了");
    // 没有对应译文的键：照旧显示后端给的文字
    expect(formatError({ code: "ssh.failed", params: { stderr_tail: "ssh 原文", stderr_tail_key: "没有这个键" } })).toBe("SSH 连接失败（ssh 原文）");
  });
  it("说明的译文可以引用同一个错误的其他参数", async () => {
    i18n.addResource("en", "translation", "errors.detail.test.with_param", "port {{port}} is busy");
    await i18n.changeLanguage("en");
    expect(formatError({ code: "sync.archive_io", params: { reason: "端口 22 被占用", reason_key: "test.with_param", port: "22" } }))
      .toBe("Couldn't write to the conflict archive (port 22 is busy)");
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
