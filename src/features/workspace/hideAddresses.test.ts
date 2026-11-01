import { describe, expect, it } from "vitest";
import { MASK, maskHost } from "./hideAddresses";

describe("隐藏 IP 时遮住地址", () => {
  it("遮住地址里的主机，用户名和端口留着", () => {
    expect(maskHost("dev@203.0.113.18", "203.0.113.18")).toBe(`dev@${MASK}`);
    expect(maskHost("admin@192.0.2.12:2222", "192.0.2.12")).toBe(`admin@${MASK}:2222`);
    expect(maskHost("ops@gpu.example.com", "gpu.example.com")).toBe(`ops@${MASK}`);
    expect(maskHost("root@2001:db8::1", "2001:db8::1")).toBe(`root@${MASK}`);
  });

  it("名称就是 IP 或含有 IP 时也遮住，普通名称不动", () => {
    expect(maskHost("203.0.113.18", "203.0.113.18")).toBe(MASK);
    expect(maskHost("GPU.example.com", "gpu.example.com")).toBe(MASK);
    expect(maskHost("hk-198.51.100.7", null)).toBe(`hk-${MASK}`);
    expect(maskHost("gpu-dev", "203.0.113.18")).toBe("gpu-dev");
    expect(maskHost("gpu-a100", null)).toBe("gpu-a100");
  });

  it("主机名只按整段遮：前后紧挨着字母或数字的不算", () => {
    expect(maskHost("device-lab", "dev")).toBe("device-lab");
    expect(maskHost("dev-box", "dev")).toBe(`${MASK}-box`);
    expect(maskHost("prod-gpu.example.com", "gpu.example.com")).toBe(`prod-${MASK}`);
  });

  it("主机名里的正则特殊字符按原样匹配", () => {
    expect(maskHost("a+b.host", "a+b.host")).toBe(MASK);
    expect(maskHost("axb.host", "a+b.host")).toBe("axb.host");
  });
});
