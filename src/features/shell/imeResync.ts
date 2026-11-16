/**
 * 让 Windows 的输入法重新接到正在打字的输入框上。
 *
 * 窗口焦点出去又马上回来一趟之后（最常见的是点了一下窗口边框、拖边框改大小），WebView 以为输入法还接在输入框上，
 * Windows 那边实际已经退回它自带的兼容方式：拼音在屏幕左上角的一个小白框里打，打完的字不走“组字”，
 * 而是当成一个个按键送进来（终端里还会因此上屏两遍，见 terminal/imeBypass）。再点输入框也回不来，
 * 要切到别的窗口再切回来才恢复。
 *
 * 页面上给输入框换一下 inputmode（"text" 和不写在桌面上是一个意思），WebView 会向 Windows 重新登记一次，
 * 输入法就回到输入框里。所以：窗口重新得到焦点后换一次；万一还是漏了（收到了绕过输入框送来的字），再换一次。
 */

/** 窗口得到焦点后等多久再换：等焦点来回那一下过去（实测马上换也行，留一点余量） */
export const FOCUS_DELAY_MS = 50;

/** 输入法吃掉的按键：keyCode 都是这个值 */
export const IME_KEY = 229;

/** 换 inputmode 用到的最小接口：真实的输入框满足它，测试里用假的 */
export interface ImeTarget {
  getAttribute(name: string): string | null;
  setAttribute(name: string, value: string): void;
  removeAttribute(name: string): void;
}

/**
 * 给输入框换一下 inputmode：没写的写上 "text"，写着 "text" 的去掉（两者在桌面上是一个意思）。
 * 写了别的值（数字框的 "numeric" 等）的不动：换了会改掉触屏键盘的样式，这些框里也不打中文。
 * @param el 正在打字的输入框
 * @returns 换了没有
 */
export function flipInputMode(el: ImeTarget): boolean {
  const mode = el.getAttribute("inputmode");
  if (mode === null || mode === "") {
    el.setAttribute("inputmode", "text");
    return true;
  }
  if (mode.toLowerCase() === "text") {
    el.removeAttribute("inputmode");
    return true;
  }
  return false;
}

/** 重新接入要用到的外部环境（测试里换成假的） */
export interface ImeResyncEnv {
  /** 现在正在打字的输入框：窗口没有焦点、焦点不在能打字的地方时为 null */
  target(): ImeTarget | null;
  setTimer(run: () => void, ms: number): unknown;
  clearTimer(id: unknown): void;
}

export interface ImeResync {
  /** 窗口得到了焦点 */
  windowFocused(): void;
  /** 开始组字（输入法接在输入框上时才有） */
  compositionStart(): void;
  /** 组字结束 */
  compositionEnd(): void;
  /** 按下了一个键 */
  keyDown(keyCode: number): void;
  /**
   * 收到一个 keypress。
   * @returns true：这是输入法绕过输入框送来的字（跟在它吃掉的按键后面），说明输入法没接在输入框上
   */
  keyPress(charCode: number): boolean;
  /** 取消还没做的那一次 */
  dispose(): void;
}

/**
 * 什么时候给输入框换 inputmode。和页面无关，页面把事件报进来。
 * 正在组字时不换：换了会打断正在打的字（能组字也说明输入法接得好好的）。
 * @param env 外部环境
 */
export function createImeResync(env: ImeResyncEnv): ImeResync {
  let composing = false;
  let afterImeKey = false;
  let timer: unknown = null;
  const run = () => {
    timer = null;
    if (composing) return;
    const el = env.target();
    if (el) flipInputMode(el);
  };
  const soon = (ms: number) => {
    if (timer !== null) env.clearTimer(timer);
    timer = env.setTimer(run, ms);
  };
  return {
    windowFocused: () => soon(FOCUS_DELAY_MS),
    compositionStart: () => { composing = true; },
    compositionEnd: () => { composing = false; },
    keyDown: (keyCode) => { afterImeKey = keyCode === IME_KEY; },
    keyPress: (charCode) => {
      if (!afterImeKey || charCode < 0x20) return false;
      // 一个词的几个字连着来：等这一串过去再换
      soon(0);
      return true;
    },
    dispose: () => {
      if (timer !== null) env.clearTimer(timer);
      timer = null;
    },
  };
}

/** 能打字的输入框：多行文本框、文字类的单行框、可编辑区域 */
function textEntry(el: Element | null): HTMLElement | null {
  if (el instanceof HTMLTextAreaElement) return el;
  if (el instanceof HTMLInputElement) return ["text", "search", "url", "email", "password", "tel", ""].includes(el.type) ? el : null;
  return el instanceof HTMLElement && el.isContentEditable ? el : null;
}

/**
 * 在整个页面上装好：窗口得到焦点后、收到绕过输入框送来的字后，让输入法重新接到当前输入框上。
 * @returns 卸掉的函数
 */
export function installImeResync(): () => void {
  const resync = createImeResync({
    target: () => (document.hasFocus() ? textEntry(document.activeElement) : null),
    setTimer: (run, ms) => window.setTimeout(run, ms),
    clearTimer: (id) => window.clearTimeout(id as number),
  });
  const onFocus = () => resync.windowFocused();
  const onStart = () => resync.compositionStart();
  const onEnd = () => resync.compositionEnd();
  const onDown = (e: KeyboardEvent) => resync.keyDown(e.keyCode);
  const onPress = (e: KeyboardEvent) => { resync.keyPress(e.charCode); };
  window.addEventListener("focus", onFocus);
  // 捕获阶段：终端会拦下这些事件自己处理，这里要先看到
  document.addEventListener("compositionstart", onStart, true);
  document.addEventListener("compositionend", onEnd, true);
  document.addEventListener("keydown", onDown, true);
  document.addEventListener("keypress", onPress, true);
  return () => {
    resync.dispose();
    window.removeEventListener("focus", onFocus);
    document.removeEventListener("compositionstart", onStart, true);
    document.removeEventListener("compositionend", onEnd, true);
    document.removeEventListener("keydown", onDown, true);
    document.removeEventListener("keypress", onPress, true);
  };
}
