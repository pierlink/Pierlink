/**
 * 组合键的表示、解析和显示。
 *
 * 保存和比较都用规范写法：修饰键按 Ctrl、Alt、Shift、Meta 的顺序，最后是按键的物理位置（KeyboardEvent.code），
 * 用“+”连起来，如 "Ctrl+Shift+KeyO"、"Ctrl+Tab"、"Ctrl+Equal"。
 * 按物理位置识别：中文输入法开着时 KeyboardEvent.key 是 "Process"，靠不住。
 */

/** 一次按键：按着哪些修饰键、按的是哪个键（只按了修饰键时 code 为 null） */
export interface Chord {
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  meta: boolean;
  code: string | null;
}

/** 修饰键本身的物理位置和 key（只按了修饰键时用来认出来） */
const MODIFIER_CODES = new Set(["ControlLeft", "ControlRight", "ShiftLeft", "ShiftRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight", "OSLeft", "OSRight"]);
const MODIFIER_KEYS = new Set(["Control", "Shift", "Alt", "AltGraph", "Meta", "OS"]);

/** 能当快捷键的按键（物理位置）和界面上显示的文字（按美式键盘写） */
const KEY_LABELS: Readonly<Record<string, string>> = (() => {
  const m: Record<string, string> = {};
  for (let i = 0; i < 26; i++) {
    const c = String.fromCharCode(65 + i);
    m[`Key${c}`] = c;
  }
  for (let i = 0; i < 10; i++) {
    m[`Digit${i}`] = String(i);
    m[`Numpad${i}`] = `Num ${i}`;
  }
  for (let i = 1; i <= 24; i++) m[`F${i}`] = `F${i}`;
  Object.assign(m, {
    Minus: "-", Equal: "=", BracketLeft: "[", BracketRight: "]", Backslash: "\\", Semicolon: ";", Quote: "'", Backquote: "`",
    Comma: ",", Period: ".", Slash: "/",
    Space: "Space", Tab: "Tab", Enter: "Enter", Backspace: "Backspace", Delete: "Delete", Insert: "Insert", Escape: "Esc",
    Home: "Home", End: "End", PageUp: "PageUp", PageDown: "PageDown",
    ArrowUp: "↑", ArrowDown: "↓", ArrowLeft: "←", ArrowRight: "→",
    NumpadAdd: "Num +", NumpadSubtract: "Num -", NumpadMultiply: "Num *", NumpadDivide: "Num /", NumpadDecimal: "Num .", NumpadEnter: "Num Enter",
  });
  return m;
})();

/** 解析时认的按键名（不分大小写）：物理位置名、显示的文字，外加几个常见写法 */
const CODE_BY_NAME: ReadonlyMap<string, string> = (() => {
  const m = new Map<string, string>();
  for (const code of Object.keys(KEY_LABELS)) m.set(code.toLowerCase(), code);
  for (const [code, label] of Object.entries(KEY_LABELS)) if (!m.has(label.toLowerCase())) m.set(label.toLowerCase(), code);
  const aliases: [string, string][] = [
    ["esc", "Escape"], ["del", "Delete"], ["ins", "Insert"], ["pgup", "PageUp"], ["pgdn", "PageDown"], ["return", "Enter"],
    ["up", "ArrowUp"], ["down", "ArrowDown"], ["left", "ArrowLeft"], ["right", "ArrowRight"],
  ];
  for (const [name, code] of aliases) m.set(name, code);
  return m;
})();

const MODIFIER_NAMES: Readonly<Record<string, "ctrl" | "alt" | "shift" | "meta">> = {
  ctrl: "ctrl", control: "ctrl", alt: "alt", shift: "shift", meta: "meta", win: "meta", cmd: "meta",
};

/**
 * 把组合键文字解析成按键（修饰键不分先后、不分大小写；按键可以写物理位置名或显示的文字）。
 * @param text 如 "Ctrl+Shift+KeyO"、"ctrl+shift+o"、"Ctrl+="
 * @returns 解析结果；不是合法的组合键（没有按键、按键不认识、修饰键重复）时为 null
 */
export function parseCombo(text: string): Chord | null {
  if (typeof text !== "string" || !text || text.length > 64) return null;
  // “Ctrl++”这种写法：最后一个“+”是按键本身
  const parts = text.endsWith("++") ? [...text.slice(0, -2).split("+"), "+"] : text.split("+");
  const chord: Chord = { ctrl: false, alt: false, shift: false, meta: false, code: null };
  for (let i = 0; i < parts.length; i++) {
    const name = parts[i].trim().toLowerCase();
    if (!name) return null;
    if (i < parts.length - 1) {
      const mod = MODIFIER_NAMES[name];
      if (!mod || chord[mod]) return null;
      chord[mod] = true;
      continue;
    }
    // “+”在美式键盘上是 Shift+=
    if (name === "+") {
      chord.shift = true;
      chord.code = "Equal";
      continue;
    }
    const code = CODE_BY_NAME.get(name);
    if (!code) return null;
    chord.code = code;
  }
  return chord;
}

/**
 * 按键的规范写法。
 * @param c 按键
 * @returns 如 "Ctrl+Shift+KeyO"；只按了修饰键时为 null
 */
export function comboOf(c: Chord): string | null {
  if (!c.code) return null;
  return [c.ctrl && "Ctrl", c.alt && "Alt", c.shift && "Shift", c.meta && "Meta", c.code].filter(Boolean).join("+");
}

/**
 * 把任意写法整理成规范写法。
 * @param text 读到的组合键文字（可能来自旧设置或被手改过）
 * @returns 规范写法；不是合法的组合键时为 null
 */
export function normalizeCombo(text: unknown): string | null {
  const c = typeof text === "string" ? parseCombo(text) : null;
  return c ? comboOf(c) : null;
}

/**
 * 从键盘事件读出按键。
 * @param e 键盘事件（只用到修饰键、code、key）
 * @returns 按键；按的是 AltGr（欧洲键盘上打 @、€ 等字符，浏览器同时报 Ctrl 和 Alt）或读不出物理位置时为 null
 */
export function chordFromEvent(e: Pick<KeyboardEvent, "code" | "key" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey"> & { getModifierState?: (key: string) => boolean }): Chord | null {
  if (e.getModifierState?.("AltGraph")) return null;
  const chord: Chord = { ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey, meta: e.metaKey, code: null };
  if (MODIFIER_CODES.has(e.code) || MODIFIER_KEYS.has(e.key)) return chord;
  if (!e.code) return null;
  return { ...chord, code: e.code };
}

/** 修饰键在界面上的写法 */
function modifierLabels(c: Chord): string[] {
  return [c.ctrl && "Ctrl", c.alt && "Alt", c.shift && "Shift", c.meta && "Win"].filter((x): x is string => !!x);
}

/**
 * 组合键在界面上的写法。
 * @param combo 规范写法；null 表示没有快捷键
 * @param sep 连接符（菜单里用空格）
 * @returns 如 "Ctrl+Shift+O"、"Ctrl+="；没有快捷键或写法不对时为空字符串
 */
export function formatCombo(combo: string | null, sep = "+"): string {
  const c = combo ? parseCombo(combo) : null;
  if (!c?.code) return "";
  return [...modifierLabels(c), KEY_LABELS[c.code] ?? c.code].join(sep);
}

/**
 * 录制时已经按下的修饰键，如 "Ctrl+Shift+"；什么都没按时为空字符串。
 * @param c 按键（只看修饰键）
 */
export function formatHeld(c: Chord): string {
  const mods = modifierLabels(c);
  return mods.length ? `${mods.join("+")}+` : "";
}

/** 不能当快捷键的原因：只按了修饰键、要带 Ctrl 或 Alt、不能用 Win 键、不支持这个键 */
export type ComboProblem = "modifierOnly" | "needModifier" | "winKey" | "unsupported";

/**
 * 检查一个按键能不能当快捷键（不管和谁冲突）。
 * 要带 Ctrl 或 Alt：只按字母、数字或只加 Shift 是在打字；F1～F24 可以单按。Win 键的组合归系统。
 * @param c 按键
 * @returns 不能用的原因；能用时为 null
 */
export function comboProblem(c: Chord): ComboProblem | null {
  if (!c.code) return "modifierOnly";
  if (!(c.code in KEY_LABELS)) return "unsupported";
  if (c.meta) return "winKey";
  if (!c.ctrl && !c.alt && !/^F\d+$/.test(c.code)) return "needModifier";
  return null;
}

/** 带修饰键时终端会发自己的序列的键：方向键、Home/End、翻页、Delete、回车、退格、Esc */
const NAV_CODES = new Set(["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End", "PageUp", "PageDown", "Delete", "Enter", "Backspace", "Escape"]);
/** Alt 加上这些键，终端发 Esc 前缀（readline、emacs 的 Meta 键） */
const ALT_CODES = /^(Key[A-Z]|Digit\d|Minus|Equal|BracketLeft|BracketRight|Backslash|Semicolon|Quote|Backquote|Comma|Period|Slash|Space)$/;
/** 只按 Ctrl 加上这些键，终端发控制字符（Ctrl+C 中断、Ctrl+R 搜历史、Ctrl+Space 是 Ctrl+@……） */
const CTRL_CODES = /^(Key[A-Z]|Space|Digit[3-8]|BracketLeft|BracketRight|Backslash)$/;

/**
 * 终端里的程序会不会用到这个键：占用后，焦点在终端里时这个键不再发给程序。
 * 按 xterm.js 6 的键盘处理：Ctrl+字母等控制字符、Alt 开头的 Meta 键、F1～F12、带修饰键的方向键等都会发；
 * Ctrl+Shift+字母、Ctrl+Tab、Ctrl+=、Ctrl+-、Ctrl+0、Ctrl+, 不发任何东西。
 * @param c 按键
 * @returns 会用到时为 true
 */
export function terminalUses(c: Chord): boolean {
  const code = c.code ?? "";
  if (/^F([1-9]|1[0-2])$/.test(code)) return true;
  if (NAV_CODES.has(code)) return true;
  if (c.alt && ALT_CODES.test(code)) return true;
  if (c.ctrl && !c.shift && CTRL_CODES.test(code)) return true;
  // 美式键盘上 Ctrl+Shift+- 是 Ctrl+_（readline 的撤销），Ctrl+Shift+2 是 Ctrl+@
  if (c.ctrl && c.shift && (code === "Minus" || code === "Digit2")) return true;
  return false;
}
