import { describe, expect, it } from "vitest";
import { createOscGuard } from "./oscGuard";

const enc = new TextEncoder();
const dec = new TextDecoder();
/** 把 text 按每块 size 个字节喂进去，返回拼起来的结果 */
function feed(text: string, size: number): string {
  const guard = createOscGuard();
  const bytes = enc.encode(text);
  const parts: number[] = [];
  for (let i = 0; i < bytes.length; i += size) parts.push(...guard(bytes.subarray(i, i + size)));
  return dec.decode(new Uint8Array(parts));
}

describe("标题里夹着的 C1 控制字符", () => {
  // screen 4.09 存坏的中文标题，重新接回会话时发出来的样子
  const broken = "tokens\x1b]0;3 docs/12 \x1f#ºÁ\u009a\x0e¤¥\x07after";
  const cleaned = "tokens\x1b]0;3 docs/12 \x1f#ºÁ\x0e¤¥\x07after";

  it("去掉标题里的 C1 控制字符，其余原样", () => {
    expect(feed(broken, 4096)).toBe(cleaned);
  });

  it("输出不管在哪里被切成几块，结果都一样", () => {
    const bytes = enc.encode(broken).length;
    for (let size = 1; size <= bytes; size++) expect(feed(broken, size)).toBe(cleaned);
  });

  it("以 ESC \\ 结束的标题、连着两个标题也一样处理", () => {
    expect(feed("\x1b]2;a\u0085b\x1b\\\x1b]0;c\u009cd\x07", 3)).toBe("\x1b]2;ab\x1b\\\x1b]0;cd\x07");
  });

  it("标题以外的内容不动，正常的中文标题不动", () => {
    for (const text of ["plain \u009a text ¤¥", "\x1b[31m红\u0085\x1b[0m", "\x1b]0;量化 策略 业务 万\x07", "\x1b]0;x\x07\u009a"]) {
      for (const size of [1, 2, 3, 4096]) expect(feed(text, size)).toBe(text);
    }
  });

  it("没有要去掉的字节时返回同一个数组", () => {
    const guard = createOscGuard();
    const data = enc.encode("ls -la\r\n\x1b]0;标题\x07$ ");
    expect(guard(data)).toBe(data);
  });
});
