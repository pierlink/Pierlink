import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { resolveBindings } from "./actions";
import { handleShortcutKey, pauseShortcuts, registerShortcut, setActiveBindings } from "./runtime";

// 测试在 Node 里跑，没有 DOM：用几个最小的替身类，只实现分发逻辑用到的部分
class FakeElement {
  /** closest() 能找到的祖先选择器 */
  constructor(public inside: string[] = []) {}
  closest(sel: string) { return this.inside.includes(sel) ? this : null; }
}
class FakeHTMLElement extends FakeElement { isContentEditable = false; }
class FakeTextArea extends FakeHTMLElement {
  readOnly = false;
  disabled = false;
  constructor(public classes: string[] = []) { super(); }
  get classList() { return { contains: (c: string) => this.classes.includes(c) }; }
}
class FakeInput extends FakeHTMLElement { type = "text"; readOnly = false; disabled = false; }
class FakeSelect extends FakeHTMLElement {}
let modalOpen = false;

beforeAll(() => {
  vi.stubGlobal("Element", FakeElement);
  vi.stubGlobal("HTMLElement", FakeHTMLElement);
  vi.stubGlobal("HTMLTextAreaElement", FakeTextArea);
  vi.stubGlobal("HTMLInputElement", FakeInput);
  vi.stubGlobal("HTMLSelectElement", FakeSelect);
  vi.stubGlobal("document", { activeElement: null, querySelector: () => (modalOpen ? {} : null) });
  setActiveBindings(resolveBindings({}));
});
afterAll(() => { vi.unstubAllGlobals(); });
beforeEach(() => { modalOpen = false; });

/** 模拟一次按下：记下有没有被拦（preventDefault）、有没有停止传递（stopPropagation） */
function press(code: string, mods: { ctrl?: boolean; shift?: boolean } = {}, opts: { target?: unknown; repeat?: boolean; isComposing?: boolean } = {}) {
  const e = {
    code, key: "x", ctrlKey: !!mods.ctrl, altKey: false, shiftKey: !!mods.shift, metaKey: false,
    getModifierState: () => false,
    repeat: !!opts.repeat, isComposing: !!opts.isComposing, defaultPrevented: false,
    target: opts.target ?? new FakeHTMLElement(),
    stopped: false,
    preventDefault() { this.defaultPrevented = true; },
    stopPropagation() { this.stopped = true; },
  };
  handleShortcutKey(e as unknown as KeyboardEvent);
  return { prevented: e.defaultPrevented, stopped: e.stopped };
}
const CS = { ctrl: true, shift: true };

describe("全局按键分发", () => {
  it("匹配到快捷键、有地方执行：执行并拦下，不再往下传", () => {
    const run = vi.fn();
    const off = registerShortcut("toggleFiles", run);
    expect(press("KeyE", CS)).toEqual({ prevented: true, stopped: true });
    expect(run).toHaveBeenCalledTimes(1);
    off();
  });
  it("不是快捷键的键原样放过（终端里的 Ctrl+C）", () => {
    const run = vi.fn();
    const off = registerShortcut("toggleFiles", run);
    expect(press("KeyC", { ctrl: true })).toEqual({ prevented: false, stopped: false });
    expect(run).not.toHaveBeenCalled();
    off();
  });
  it("当前页面没有这个动作：只挡住 WebView 自带的反应，照常往下传", () => {
    expect(press("KeyT", CS)).toEqual({ prevented: true, stopped: false });
  });
  it("焦点在终端里也触发；在输入框、多行文本框里不触发", () => {
    const run = vi.fn();
    const off = registerShortcut("newTab", run);
    expect(press("KeyT", CS, { target: new FakeTextArea(["xterm-helper-textarea"]) }).stopped).toBe(true);
    expect(press("KeyT", CS, { target: new FakeInput() })).toEqual({ prevented: false, stopped: false });
    expect(press("KeyT", CS, { target: new FakeTextArea() })).toEqual({ prevented: false, stopped: false });
    // 只读的输入框不算在输入
    const readOnly = new FakeInput();
    readOnly.readOnly = true;
    expect(press("KeyT", CS, { target: readOnly }).stopped).toBe(true);
    expect(run).toHaveBeenCalledTimes(2);
    off();
  });
  it("开着对话框或菜单、焦点在远程桌面画面里时不触发", () => {
    const run = vi.fn();
    const off = registerShortcut("nextTab", run);
    modalOpen = true;
    expect(press("Tab", { ctrl: true })).toEqual({ prevented: false, stopped: false });
    modalOpen = false;
    expect(press("Tab", { ctrl: true }, { target: new FakeElement([".desk__screen"]) })).toEqual({ prevented: false, stopped: false });
    expect(run).not.toHaveBeenCalled();
    off();
  });
  it("录制快捷键时整个暂停，恢复后照常", () => {
    const run = vi.fn();
    const off = registerShortcut("toggleFiles", run);
    const resume = pauseShortcuts();
    expect(press("KeyE", CS)).toEqual({ prevented: false, stopped: false });
    resume();
    resume();
    expect(press("KeyE", CS).stopped).toBe(true);
    expect(run).toHaveBeenCalledTimes(1);
    off();
  });
  it("输入法组字中不触发", () => {
    const run = vi.fn();
    const off = registerShortcut("toggleFiles", run);
    expect(press("KeyE", CS, { isComposing: true })).toEqual({ prevented: false, stopped: false });
    expect(run).not.toHaveBeenCalled();
    off();
  });
  it("按住不放：切标签连续执行，开关类只算一次（后面的仍然拦下）", () => {
    const files = vi.fn();
    const next = vi.fn();
    const offs = [registerShortcut("toggleFiles", files), registerShortcut("nextTab", next)];
    press("KeyE", CS);
    expect(press("KeyE", CS, { repeat: true })).toEqual({ prevented: true, stopped: true });
    press("Tab", { ctrl: true });
    press("Tab", { ctrl: true }, { repeat: true });
    expect(files).toHaveBeenCalledTimes(1);
    expect(next).toHaveBeenCalledTimes(2);
    offs.forEach((f) => f());
  });
  it("同一个动作登记了多处：后登记的执行，取消后回到前一个", () => {
    const first = vi.fn();
    const second = vi.fn();
    const offFirst = registerShortcut("toggleFiles", first);
    const offSecond = registerShortcut("toggleFiles", second);
    press("KeyE", CS);
    offSecond();
    press("KeyE", CS);
    expect(second).toHaveBeenCalledTimes(1);
    expect(first).toHaveBeenCalledTimes(1);
    offFirst();
    expect(press("KeyE", CS).stopped).toBe(false);
  });
});
