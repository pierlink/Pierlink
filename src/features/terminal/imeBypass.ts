import { IME_KEY } from "../shell/imeResync";

/**
 * 输入法没接在输入框上时（原因和补救见 shell/imeResync），打完的字不走“组字”，而是跟在输入法吃掉的那个按键
 * （keyCode 229）后面，一个字一个 keypress 送进来。xterm 遇到这种 keypress 会把字发两遍：keypress 本身发一遍；
 * 浏览器又把字写进了隐藏输入框，xterm 在那个 229 按键之后安排的“看看输入框多了什么”再发一遍
 * （哪一遍先到看时机，所以有的词重复、有的不重复）。
 * 这里把这种 keypress 接过来：字只发一遍，调用方不再交给 xterm，也不让浏览器写进输入框。
 */
export interface ImeBypass {
  /** 按下了一个键 */
  keyDown(keyCode: number): void;
  /**
   * 收到一个 keypress。
   * @param charCode 事件的 charCode
   * @param key 事件的 key（这个字本身）
   * @returns true：是输入法绕过输入框送来的字，已经发出去（或先存着等下半个），调用方要拦下这个事件
   */
  keyPress(charCode: number, key?: string): boolean;
}

/**
 * @param send 把字发给终端里的程序
 */
export function createImeBypass(send: (text: string) => void): ImeBypass {
  let afterImeKey = false;
  // 基本平面以外的字（部分生僻字、表情）：有时整个字在 key 里（charCode 只有上半个），有时分成两个 keypress 来——
  // 先存上半个，等下半个到了一起发
  let high = "";
  return {
    keyDown: (keyCode) => {
      afterImeKey = keyCode === IME_KEY;
      high = "";
    },
    keyPress: (charCode, key) => {
      if (!afterImeKey || charCode < 0x20 || charCode === 0x7f) return false;
      if (key && key.length === 2 && (key.codePointAt(0) ?? 0) > 0xffff) {
        high = "";
        send(key);
        return true;
      }
      if (charCode > 0xffff) {
        high = "";
        send(String.fromCodePoint(charCode));
        return true;
      }
      const ch = String.fromCharCode(charCode);
      if (charCode >= 0xd800 && charCode <= 0xdbff) {
        high = ch;
        return true;
      }
      if (charCode >= 0xdc00 && charCode <= 0xdfff) {
        // 单独来的下半个凑不成字：丢掉
        if (high) send(high + ch);
        high = "";
        return true;
      }
      high = "";
      send(ch);
      return true;
    },
  };
}
