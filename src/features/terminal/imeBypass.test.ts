import { describe, expect, it } from "vitest";
import { createImeBypass } from "./imeBypass";

/** 输入法吃掉的按键 */
const IME = 229;

function setup() {
  const sent: string[] = [];
  return { sent, bypass: createImeBypass((text) => sent.push(text)) };
}

describe("输入法绕过输入框送来的字", () => {
  it("跟在输入法吃掉的按键后面的 keypress：接过来，一个字发一遍", () => {
    const { sent, bypass } = setup();
    bypass.keyDown(IME);
    expect(bypass.keyPress("测".charCodeAt(0))).toBe(true);
    expect(bypass.keyPress("试".charCodeAt(0))).toBe(true);
    expect(sent).toEqual(["测", "试"]);
  });

  it("回车上屏的拼音字母、标点也算", () => {
    const { sent, bypass } = setup();
    bypass.keyDown(IME);
    for (const ch of "ni，") expect(bypass.keyPress(ch.charCodeAt(0))).toBe(true);
    expect(sent).toEqual(["n", "i", "，"]);
  });

  it("普通按键后面的 keypress 不接（照旧交给 xterm）", () => {
    const { sent, bypass } = setup();
    bypass.keyDown(65);
    expect(bypass.keyPress(65)).toBe(false);
    expect(sent).toEqual([]);
  });

  it("中间按过别的键就不算了", () => {
    const { sent, bypass } = setup();
    bypass.keyDown(IME);
    bypass.keyDown(16);
    expect(bypass.keyPress("字".charCodeAt(0))).toBe(false);
    expect(sent).toEqual([]);
  });

  it("没按过键就来的 keypress 不接", () => {
    const { sent, bypass } = setup();
    expect(bypass.keyPress("字".charCodeAt(0))).toBe(false);
    expect(sent).toEqual([]);
  });

  it("控制字符不接", () => {
    const { sent, bypass } = setup();
    bypass.keyDown(IME);
    expect(bypass.keyPress(13)).toBe(false);
    expect(bypass.keyPress(0x7f)).toBe(false);
    expect(bypass.keyPress(0)).toBe(false);
    expect(sent).toEqual([]);
  });

  it("分成两个 keypress 来的字拼好再发", () => {
    const { sent, bypass } = setup();
    const emoji = "😀";
    bypass.keyDown(IME);
    expect(bypass.keyPress(emoji.charCodeAt(0))).toBe(true);
    expect(sent).toEqual([]);
    expect(bypass.keyPress(emoji.charCodeAt(1))).toBe(true);
    expect(sent).toEqual([emoji]);
  });

  it("整个字在 key 里、charCode 只有上半个：按 key 发", () => {
    const { sent, bypass } = setup();
    const emoji = "😀";
    bypass.keyDown(IME);
    expect(bypass.keyPress(emoji.charCodeAt(0), emoji)).toBe(true);
    expect(sent).toEqual([emoji]);
    // 后面的字不受影响
    expect(bypass.keyPress("好".charCodeAt(0), "好")).toBe(true);
    expect(sent).toEqual([emoji, "好"]);
  });

  it("key 里只有半个字时照旧等下半个", () => {
    const { sent, bypass } = setup();
    const emoji = "😀";
    bypass.keyDown(IME);
    expect(bypass.keyPress(emoji.charCodeAt(0), emoji[0])).toBe(true);
    expect(bypass.keyPress(emoji.charCodeAt(1), emoji[1])).toBe(true);
    expect(sent).toEqual([emoji]);
  });

  it("一个 keypress 就带着完整码点的字直接发", () => {
    const { sent, bypass } = setup();
    bypass.keyDown(IME);
    expect(bypass.keyPress(0x20bb7)).toBe(true);
    expect(sent).toEqual(["𠮷"]);
  });

  it("只有上半个：后面来了普通字，上半个丢掉", () => {
    const { sent, bypass } = setup();
    bypass.keyDown(IME);
    bypass.keyPress(0xd83d);
    expect(bypass.keyPress("好".charCodeAt(0))).toBe(true);
    expect(sent).toEqual(["好"]);
    // 之后单独来的下半个也凑不成字
    expect(bypass.keyPress(0xde00)).toBe(true);
    expect(sent).toEqual(["好"]);
  });

  it("上半个没等到下半个、又按了键：不留到下一次", () => {
    const { sent, bypass } = setup();
    bypass.keyDown(IME);
    bypass.keyPress(0xd83d);
    bypass.keyDown(IME);
    expect(bypass.keyPress(0xde00)).toBe(true);
    expect(sent).toEqual([]);
  });
});
