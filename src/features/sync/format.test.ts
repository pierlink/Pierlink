import { describe, expect, it } from "vitest";
import { formatBytes, formatCount } from "./format";

describe("formatBytes", () => {
  it("按 1024 进制换算并保留一位小数", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1023)).toBe("1023 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(46 * 1024 * 1024)).toBe("46 MB");
    expect(formatBytes(1.34 * 1024 ** 3)).toBe("1.3 GB");
    expect(formatBytes(412 * 1024 * 1024)).toBe("412 MB");
  });
  it("非法值显示占位符", () => {
    expect(formatBytes(-1)).toBe("-");
    expect(formatBytes(Number.NaN)).toBe("-");
  });
});

describe("formatCount", () => {
  it("加千位分隔符", () => {
    expect(formatCount(31906, "en")).toBe("31,906");
  });
});

describe("盘符大写", () => {
  it("小写盘符改成大写，其他路径不变", async () => {
    const { upperDrive } = await import("./format");
    expect(upperDrive("d:\\work\\demo")).toBe("D:\\work\\demo");
    expect(upperDrive("C:\\work")).toBe("C:\\work");
    expect(upperDrive("/home/u")).toBe("/home/u");
    expect(upperDrive("\\\\server\\share")).toBe("\\\\server\\share");
  });
});
