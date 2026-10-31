import { describe, expect, it } from "vitest";
import { parseBatch, targetKey } from "./batch";

describe("parseBatch", () => {
  it("解析地址、端口、名称和分组，跳过空行和注释", () => {
    const r = parseBatch("# 注释\n\ndev@10.0.0.1:2222 gpu-a 实验室 A\nroot@web.example.com\n  ops@[::1]:22 local  ");
    expect(r.map((x) => x.server)).toEqual([
      { name: "gpu-a", host: "10.0.0.1", port: 2222, username: "dev", groupName: "实验室 A" },
      { name: "web.example.com", host: "web.example.com", port: 22, username: "root", groupName: null },
      { name: "local", host: "::1", port: 22, username: "ops", groupName: null },
    ]);
    expect(r.map((x) => x.line)).toEqual([3, 4, 5]);
  });
  it("格式错误、端口越界、重复都标出来", () => {
    const r = parseBatch("a@b:99999\ndev@@host\ndev@h\nDev@h\ndev@H", [targetKey("old", "srv", 22)]);
    expect(r.map((x) => x.error)).toEqual(["port", "format", null, null, "duplicate"]);
    expect(parseBatch("old@SRV", [targetKey("old", "srv", 22)])[0].error).toBe("duplicate");
  });
});
