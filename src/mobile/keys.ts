import type { InputRequest } from "./api";

/**
 * 手机上的组合键：一步是一个按键（名字和电脑端接受的一致，如 Ctrl+A、ArrowUp）或一段直接打进去的字。
 * 几步连起来就是一组键，比如退出 screen 是 Ctrl+A 再按 d。
 */
export type KeyStep = { key: string } | { text: string };
/** 存在键盘栏上的一组键 */
export interface KeyPreset { steps: KeyStep[] }

export type Mod = "Ctrl" | "Alt" | "Shift";
export const MODS: readonly Mod[] = ["Ctrl", "Alt", "Shift"];

/** 一组键最多几步、键盘栏上最多存几组 */
export const MAX_STEPS = 8;
export const MAX_PRESETS = 24;

const LETTERS = "abcdefghijklmnopqrstuvwxyz".split("");
const DIGITS = "1234567890".split("");
/** 能和 Ctrl 组合的符号 */
const SYMBOLS = ["]", "\\", "^", "_", "/"];
const ARROWS = ["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"];
const FUNCTION_KEYS = Array.from({ length: 12 }, (_, i) => `F${i + 1}`);

/** 面板上的键，按行分组（base 是不带修饰键的名字） */
export const KEY_ROWS: readonly (readonly string[])[] = [
  ["Escape", "Tab", "Enter", "Space", "Backspace", "Delete", "Insert"],
  [...ARROWS, "Home", "End", "PageUp", "PageDown"],
  LETTERS,
  [...DIGITS, ...SYMBOLS],
  FUNCTION_KEYS,
];

/** 没有修饰键时本身就是按键的（其余的字母、数字、符号、空格直接当字打进去） */
const PLAIN_KEYS = new Set(["Escape", "Tab", "Enter", "Backspace", "Delete", "Insert", ...ARROWS, "Home", "End", "PageUp", "PageDown", ...FUNCTION_KEYS]);
/** 每个修饰键能配哪些键（电脑端认得的组合） */
const WITH_MOD: Record<Mod, Set<string>> = {
  Ctrl: new Set([...LETTERS, ...SYMBOLS, "Enter", "Space", "Backspace", "Delete", ...ARROWS, "Home", "End"]),
  Alt: new Set([...LETTERS, "Enter", "Backspace", ...ARROWS]),
  Shift: new Set([...LETTERS, "Tab", "Enter", ...ARROWS, "Home", "End"]),
};

/**
 * 按着 mods 点了 base 这个键，得到要发的一步；这个组合电脑端不认得时返回 null（面板上置灰）。
 * 不带修饰键的字母数字符号是直接打进去的字，Shift+字母是大写字母。
 */
export function stepFor(mods: readonly Mod[], base: string): KeyStep | null {
  if (mods.length > 1) return null;
  const mod = mods[0];
  if (!mod) {
    if (PLAIN_KEYS.has(base)) return { key: base };
    return { text: base === "Space" ? " " : base };
  }
  if (!WITH_MOD[mod].has(base)) return null;
  if (base.length === 1 && /[a-z]/.test(base)) {
    return mod === "Shift" ? { text: base.toUpperCase() } : { key: `${mod}+${base.toUpperCase()}` };
  }
  return { key: `${mod}+${base}` };
}

const BASE_LABELS: Record<string, string> = {
  Escape: "Esc", Enter: "⏎", Backspace: "⌫", Delete: "Del", Insert: "Ins", Space: "␣",
  ArrowUp: "↑", ArrowDown: "↓", ArrowLeft: "←", ArrowRight: "→", PageUp: "PgUp", PageDown: "PgDn",
};

/** 面板上一个键显示的字 */
export function baseLabel(base: string): string {
  return BASE_LABELS[base] ?? (base.length === 1 ? base.toUpperCase() : base);
}

/** 一步显示的字：Ctrl+A 写成 ^A，Shift+Tab 写成 ⇧Tab，直接打的字原样（空格写成 ␣） */
export function stepLabel(step: KeyStep): string {
  if ("text" in step) return step.text.replace(/ /g, "␣");
  const at = step.key.indexOf("+");
  // 只有 Ctrl、Alt、Shift 开头才是组合键（单独的键名里没有加号）
  if (at <= 0) return baseLabel(step.key);
  const mod = step.key.slice(0, at);
  const base = baseLabel(step.key.slice(at + 1));
  if (mod === "Ctrl") return `^${base}`;
  if (mod === "Shift") return `⇧${base}`;
  return `${mod}+${base}`;
}

/** 一组键显示的字，如 “^A d” */
export function presetLabel(steps: readonly KeyStep[]): string {
  return steps.map(stepLabel).join(" ");
}

/** 回车和会被当成回车的键：电脑要先看到屏幕上的命令才能核对，所以它们不和前面的键合成一次请求 */
const SUBMIT_KEYS = new Set(["Enter", "Ctrl+J", "Ctrl+M", "Ctrl+O", "Shift+Enter", "Ctrl+Enter", "Alt+Enter"]);

/** 这次请求是不是从回车（或会被当成回车的键）开始 */
export function startsWithSubmit(request: InputRequest): boolean {
  return !!request.keys && SUBMIT_KEYS.has(request.keys[0]);
}

/** 一组键换成依次发出的请求：连着的按键合成一次（回车另起一次），字单独一次 */
export function toRequests(steps: readonly KeyStep[]): InputRequest[] {
  const out: InputRequest[] = [];
  for (const step of steps) {
    const last = out[out.length - 1];
    if ("key" in step) {
      if (last?.keys && !SUBMIT_KEYS.has(step.key)) last.keys.push(step.key);
      else out.push({ keys: [step.key] });
    } else if (last?.text !== undefined) last.text += step.text;
    else out.push({ text: step.text });
  }
  return out;
}

function validStep(raw: unknown): KeyStep | null {
  if (!raw || typeof raw !== "object") return null;
  const { key, text } = raw as { key?: unknown; text?: unknown };
  if (typeof key === "string" && /^[A-Za-z0-9+\\\]^_/]{1,24}$/.test(key)) return { key };
  // 字只收看得见的单行字符（控制字符电脑端也不收）
  if (typeof text === "string" && text.length > 0 && text.length <= 32 && !/[\u0000-\u001f\u007f]/.test(text)) return { text };
  return null;
}

/** 读出存下来的几组键：格式不对的丢掉 */
export function parsePresets(raw: unknown): KeyPreset[] | null {
  if (!Array.isArray(raw)) return null;
  const out: KeyPreset[] = [];
  for (const item of raw.slice(0, MAX_PRESETS)) {
    const list = (item as { steps?: unknown } | null)?.steps;
    if (!Array.isArray(list) || list.length === 0 || list.length > MAX_STEPS) continue;
    const steps = list.map(validStep);
    if (steps.every((s): s is KeyStep => s !== null)) out.push({ steps });
  }
  return out;
}

/** 第一次用时键盘栏上先放的几组：结束输入、挂到后台、退出 screen、退出 tmux */
export const DEFAULT_PRESETS: readonly KeyPreset[] = [
  { steps: [{ key: "Ctrl+D" }] },
  { steps: [{ key: "Ctrl+Z" }] },
  { steps: [{ key: "Ctrl+A" }, { text: "d" }] },
  { steps: [{ key: "Ctrl+B" }, { text: "d" }] },
];
