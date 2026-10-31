import { describe, expect, it } from "vitest";
import { STEPS, stepFill, stepIndex, stepOf } from "./ConnectProgress";

describe("连接卡片的阶段", () => {
  it("后端阶段换算成界面上的四步，ready 表示完成", () => {
    expect(stepOf("resolve")).toBe("connect");
    expect(stepOf("connect")).toBe("connect");
    expect(stepOf("auth")).toBe("auth");
    expect(stepOf("session")).toBe("session");
    expect(stepOf("ready")).toBeNull();
  });

  it("四步的先后：连接、验证主机、登录、启动终端", () => {
    expect([...STEPS]).toEqual(["connect", "verify", "auth", "session"]);
    expect(STEPS.map(stepIndex)).toEqual([0, 1, 2, 3]);
  });

  it("当前一步随时间一直往前填，但不会填满", () => {
    const samples = [0, 100, 500, 1000, 3000, 10000, 60000].map(stepFill);
    expect(samples[0]).toBe(0);
    for (let i = 1; i < samples.length; i++) expect(samples[i]).toBeGreaterThan(samples[i - 1]);
    expect(samples[samples.length - 1]).toBeLessThan(1);
    expect(samples[samples.length - 1]).toBeGreaterThan(0.9);
    // 刚开始一秒就有明显的进度
    expect(stepFill(1000)).toBeGreaterThan(0.4);
    // 时钟回拨（负数）当成 0
    expect(stepFill(-50)).toBe(0);
  });
});
