import { describe, expect, it } from "vitest";
import "../../i18n";
import { endedFromOpenError, endedLine, endsInline } from "./ended";

const err = (code: string, params: Record<string, string> = {}) => ({ code, params });

describe("终端打开失败时的提示块", () => {
  it("内置 SSH 的登录类错误和系统 OpenSSH 退出时的提示一致", () => {
    expect(endedFromOpenError(err("ssh.password_needed", { engine: "builtin" }))).toEqual({ kind: "passwordNeeded" });
    expect(endedFromOpenError(err("ssh.auth_cancelled", { engine: "builtin" }))).toEqual({ kind: "passwordNeeded" });
    expect(endedFromOpenError(err("ssh.two_factor_required", { engine: "builtin" }))).toEqual({ kind: "authFailed", twoFactor: true });
    // 输的密码被拒绝：已作废，重新连接会再问，不提供改用系统 OpenSSH
    expect(endedFromOpenError(err("ssh.auth_failed", { engine: "builtin", password_generation: "3" }))).toEqual({ kind: "authFailed", password: true });
  });

  it("内置 SSH 连不上或密钥没通过：提供改用系统 OpenSSH", () => {
    expect(endedFromOpenError(err("ssh.auth_failed", { engine: "builtin" }))).toEqual({ kind: "authFailed", builtin: true });
    const failed = endedFromOpenError(err("ssh.failed", { engine: "builtin", reason: "x", stderr_tail: "x" }));
    expect(failed).toMatchObject({ kind: "failed", builtin: true });
    expect(endedFromOpenError(err("ssh.unreachable", { engine: "builtin" }))).toMatchObject({ kind: "failed", builtin: true });
  });

  it("agent 里密钥太多超过服务器的登录尝试上限：单独说明怎么改，不提供改用系统 OpenSSH（一样会撞上限）", () => {
    expect(endedFromOpenError(err("ssh.too_many_auth_attempts", { engine: "builtin" }))).toEqual({ kind: "authFailed", tooManyAuth: true });
    expect(endedFromOpenError(err("ssh.too_many_auth_attempts"))).toEqual({ kind: "authFailed", tooManyAuth: true });
  });

  it("指纹变了和系统 OpenSSH 的失败不提供改用；内置不认的指纹提供改用", () => {
    expect(endedFromOpenError(err("ssh.host_key_changed", { engine: "builtin" }))).toMatchObject({ kind: "failed" });
    expect(endedFromOpenError(err("ssh.host_key_changed", { engine: "builtin" }))).not.toHaveProperty("builtin");
    expect(endedFromOpenError(err("ssh.host_key_unknown", { engine: "builtin" }))).toMatchObject({ kind: "failed", builtin: true });
    expect(endedFromOpenError(err("ssh.failed"))).toMatchObject({ kind: "failed", builtin: false });
    expect(endedFromOpenError(new Error("x"))).toMatchObject({ kind: "failed" });
  });
});

describe("断开和退出时写进终端的一行", () => {
  it("断开、退出不盖提示块，登录失败和打不开仍然盖", () => {
    expect(endsInline({ kind: "lost" })).toBe(true);
    expect(endsInline({ kind: "disconnected" })).toBe(true);
    expect(endsInline({ kind: "exited", code: 0 })).toBe(true);
    expect(endsInline({ kind: "authFailed" })).toBe(false);
    expect(endsInline({ kind: "passwordNeeded" })).toBe(false);
    expect(endsInline({ kind: "failed", text: "x" })).toBe(false);
  });

  it("在备用屏幕里先退出备用屏幕；说明写在新的一行，最后隐藏光标", () => {
    const alt = endedLine("连接已断开", true, true);
    expect(alt.startsWith("\x1b[?1049l\x1b[!p")).toBe(true);
    expect(alt).toContain("\r\n\x1b[33m连接已断开\x1b[0m\r\n");
    expect(alt.endsWith("\x1b[?25l")).toBe(true);
    const main = endedLine("已断开", false, false);
    expect(main).not.toContain("?1049l");
    expect(main).toContain("\x1b[90m已断开");
    // 关掉鼠标上报：断开后点终端不再往外发鼠标序列
    expect(main).toContain("\x1b[?1000l");
  });
});
