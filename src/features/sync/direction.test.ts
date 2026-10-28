import { describe, expect, it } from "vitest";
import { directionOf, modesOf } from "./direction";

describe("同步方向", () => {
  it("方向和两端方式互相换算", () => {
    for (const d of ["both", "toServer", "toLocal"] as const) {
      const m = modesOf(d);
      expect(directionOf(m.localMode, [m.serverMode, m.serverMode])).toBe(d);
    }
  });
  it("服务器各端不一致、本机只接收但服务器也只接收时为 mixed", () => {
    expect(directionOf("sendReceive", ["sendReceive", "receiveOnly"])).toBe("mixed");
    expect(directionOf("receiveOnly", ["receiveOnly"])).toBe("mixed");
  });
});
