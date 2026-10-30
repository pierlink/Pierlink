import { describe, expect, it, vi } from "vitest";
import { passwordBus } from "./bus";
import { canSharePassword } from "./share";
import type { ServerRecord } from "../servers/api";

const base: ServerRecord = {
  id: "s1", name: "gpu", groupName: null, favorite: false, source: "manual", sshAlias: null, host: "203.0.113.21", port: 22,
  username: "ml", authMethod: "password", identityFile: null, loginShell: null, hasPassword: false, notes: "",
  connectionEngine: "builtin", engineNote: null, commandPrediction: true,
};

describe("共用密码", () => {
  it("应用内新建、有用户名、用密码或默认密钥登录的服务器可以共用", () => {
    expect(canSharePassword(base)).toBe(true);
    expect(canSharePassword({ ...base, authMethod: "agent" })).toBe(true);
  });

  it("导入的、没填用户名的、用指定私钥或专用密钥的不行", () => {
    expect(canSharePassword({ ...base, source: "ssh_config", sshAlias: "gpu", host: null })).toBe(false);
    expect(canSharePassword({ ...base, username: null })).toBe(false);
    expect(canSharePassword({ ...base, authMethod: "key" })).toBe(false);
    expect(canSharePassword({ ...base, authMethod: "dedicated_key" })).toBe(false);
    expect(canSharePassword(undefined)).toBe(false);
  });
});

describe("密码消息", () => {
  it("请求输入和已保存的通知送到订阅者，退订后不再收到", () => {
    const entry = vi.fn();
    const saved = vi.fn();
    const offEntry = passwordBus.onEntry(entry);
    const offSaved = passwordBus.onSaved(saved);
    passwordBus.requestEntry("s1");
    passwordBus.emitSaved("s1");
    expect(entry).toHaveBeenCalledWith("s1");
    expect(saved).toHaveBeenCalledWith("s1");
    offEntry();
    offSaved();
    passwordBus.requestEntry("s2");
    passwordBus.emitSaved("s2");
    expect(entry).toHaveBeenCalledTimes(1);
    expect(saved).toHaveBeenCalledTimes(1);
  });
});
