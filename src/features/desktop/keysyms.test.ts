import { describe, expect, it } from "vitest";
import { keysymFor, typingKeysyms } from "./keysyms";

describe("逐字输入的按键编码", () => {
  it("拉丁字符用码位，中文和表情用 Unicode keysym", () => {
    expect(keysymFor("a")).toBe(0x61);
    expect(keysymFor("Z")).toBe(0x5a);
    expect(keysymFor("é")).toBe(0xe9);
    expect(keysymFor("中")).toBe(0x01004e2d);
    expect(keysymFor("🙂")).toBe(0x0101f642);
  });

  it("换行发回车、制表符发 Tab，其他控制字符不发", () => {
    expect(keysymFor("\n")).toBe(0xff0d);
    expect(keysymFor("\t")).toBe(0xff09);
    expect(keysymFor("\u0007")).toBeNull();
    expect(keysymFor("\u0085")).toBeNull();
  });

  it("\\r\\n 和 \\r 都算一个回车，控制字符跳过", () => {
    expect(typingKeysyms("a\r\nb\rc\u0000")).toEqual([0x61, 0xff0d, 0x62, 0xff0d, 0x63]);
  });

  it("太长时返回 null", () => {
    expect(typingKeysyms("abc", 3)).toEqual([0x61, 0x62, 0x63]);
    expect(typingKeysyms("abcd", 3)).toBeNull();
  });
});
