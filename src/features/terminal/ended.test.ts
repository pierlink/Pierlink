import { describe, expect, it } from "vitest";
import "../../i18n";
import { endedFromOpenError } from "./ended";

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
