import { describe, expect, it } from "vitest";
import { createImeResync, flipInputMode, FOCUS_DELAY_MS, IME_KEY, type ImeTarget } from "./imeResync";

/** 假的输入框：只记属性 */
function fakeInput(inputmode?: string): ImeTarget & { attrs: Record<string, string>; changes: number } {
  const attrs: Record<string, string> = inputmode === undefined ? {} : { inputmode };
  const box = {
    attrs,
    changes: 0,
    getAttribute: (name: string) => (name in attrs ? attrs[name] : null),
    setAttribute: (name: string, value: string) => { attrs[name] = value; box.changes++; },
    removeAttribute: (name: string) => { delete attrs[name]; box.changes++; },
  };
  return box;
}

/** 假的定时器：记下要做的事，由测试决定什么时候到点 */
function fakeEnv(target: () => ImeTarget | null) {
  let next = 1;
  const timers = new Map<number, { run: () => void; ms: number }>();
  return {
    env: {
      target,
      setTimer: (run: () => void, ms: number) => { timers.set(next, { run, ms }); return next++; },
      clearTimer: (id: unknown) => { timers.delete(id as number); },
    },
    /** 还没到点的定时器各等多久 */
    waiting: () => [...timers.values()].map((t) => t.ms),
    /** 所有定时器到点 */
    fire: () => { const due = [...timers.values()]; timers.clear(); for (const t of due) t.run(); },
  };
}

describe("给输入框换 inputmode", () => {
  it("没写的写上 text，写着 text 的去掉", () => {
    const box = fakeInput();
    expect(flipInputMode(box)).toBe(true);
    expect(box.attrs).toEqual({ inputmode: "text" });
    expect(flipInputMode(box)).toBe(true);
    expect(box.attrs).toEqual({});
  });

  it("空值当作没写；大小写不分", () => {
    const empty = fakeInput("");
    expect(flipInputMode(empty)).toBe(true);
    expect(empty.attrs).toEqual({ inputmode: "text" });
    const upper = fakeInput("Text");
    expect(flipInputMode(upper)).toBe(true);
    expect(upper.attrs).toEqual({});
  });

  it("写了别的值的不动（数字框）", () => {
    const box = fakeInput("numeric");
    expect(flipInputMode(box)).toBe(false);
    expect(box.attrs).toEqual({ inputmode: "numeric" });
    expect(box.changes).toBe(0);
  });
});

describe("什么时候让输入法重新接入", () => {
  it("窗口得到焦点后等一小会儿换一次", () => {
    const box = fakeInput();
    const t = fakeEnv(() => box);
    const resync = createImeResync(t.env);
    resync.windowFocused();
    expect(box.changes).toBe(0);
    expect(t.waiting()).toEqual([FOCUS_DELAY_MS]);
    t.fire();
    expect(box.attrs).toEqual({ inputmode: "text" });
    expect(t.waiting()).toEqual([]);
  });

  it("连着得到几次焦点只换一次", () => {
    const box = fakeInput();
    const t = fakeEnv(() => box);
    const resync = createImeResync(t.env);
    resync.windowFocused();
    resync.windowFocused();
    resync.windowFocused();
    expect(t.waiting()).toEqual([FOCUS_DELAY_MS]);
    t.fire();
    expect(box.changes).toBe(1);
  });

  it("每次得到焦点都换，来回换的是同一个属性", () => {
    const box = fakeInput();
    const t = fakeEnv(() => box);
    const resync = createImeResync(t.env);
    resync.windowFocused();
    t.fire();
    resync.windowFocused();
    t.fire();
    expect(box.changes).toBe(2);
    expect(box.attrs).toEqual({});
  });

  it("焦点不在能打字的地方时什么都不做", () => {
    const t = fakeEnv(() => null);
    const resync = createImeResync(t.env);
    resync.windowFocused();
    expect(() => t.fire()).not.toThrow();
  });

  it("到点时才看焦点在哪个输入框", () => {
    const first = fakeInput();
    const second = fakeInput();
    let current: ImeTarget = first;
    const t = fakeEnv(() => current);
    const resync = createImeResync(t.env);
    resync.windowFocused();
    current = second;
    t.fire();
    expect(first.changes).toBe(0);
    expect(second.changes).toBe(1);
  });

  it("正在组字时不换，组字结束后下一次照常", () => {
    const box = fakeInput();
    const t = fakeEnv(() => box);
    const resync = createImeResync(t.env);
    resync.windowFocused();
    resync.compositionStart();
    t.fire();
    expect(box.changes).toBe(0);
    resync.compositionEnd();
    resync.windowFocused();
    t.fire();
    expect(box.changes).toBe(1);
  });

  it("输入法吃掉的按键后面跟着 keypress：是绕过输入框送来的字，这一串过去后换一次", () => {
    const box = fakeInput();
    const t = fakeEnv(() => box);
    const resync = createImeResync(t.env);
    resync.keyDown(IME_KEY);
    expect(resync.keyPress("测".charCodeAt(0))).toBe(true);
    expect(resync.keyPress("试".charCodeAt(0))).toBe(true);
    expect(t.waiting()).toEqual([0]);
    expect(box.changes).toBe(0);
    t.fire();
    expect(box.changes).toBe(1);
  });

  it("普通按键后面的 keypress 不算（大写字母、Alt 加小键盘打出的字）", () => {
    const box = fakeInput();
    const t = fakeEnv(() => box);
    const resync = createImeResync(t.env);
    resync.keyDown(65);
    expect(resync.keyPress(65)).toBe(false);
    resync.keyDown(IME_KEY);
    resync.keyDown(16);
    expect(resync.keyPress("字".charCodeAt(0))).toBe(false);
    expect(t.waiting()).toEqual([]);
  });

  it("控制字符不算", () => {
    const t = fakeEnv(() => fakeInput());
    const resync = createImeResync(t.env);
    resync.keyDown(IME_KEY);
    expect(resync.keyPress(13)).toBe(false);
    expect(t.waiting()).toEqual([]);
  });

  it("卸掉时取消还没做的那一次", () => {
    const box = fakeInput();
    const t = fakeEnv(() => box);
    const resync = createImeResync(t.env);
    resync.windowFocused();
    resync.dispose();
    expect(t.waiting()).toEqual([]);
    t.fire();
    expect(box.changes).toBe(0);
  });
});
