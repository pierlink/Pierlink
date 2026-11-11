import { comboOf, comboProblem, normalizeCombo, parseCombo, type Chord, type ComboProblem } from "./keys";

/** 快捷键动作在设置里分的组 */
export type ShortcutGroup = "tabs" | "panels" | "pages" | "font";

/**
 * 快捷键动作表：设置里按这个顺序列出。名字在语言文件的 shortcuts.action.<id>。
 * 默认键避开终端里程序要用的键（Ctrl+字母、Alt+字母、F1～F12、Ctrl+Shift+-），多用 Ctrl+Shift+字母（和 Windows Terminal 一样）。
 * repeat：按住不放时连续执行（切标签、调字号）；其他动作按住只算一次。
 */
export const SHORTCUT_ACTIONS = [
  { id: "newTab", group: "tabs", combo: "Ctrl+Shift+KeyT" },
  { id: "quickConnect", group: "tabs", combo: "Ctrl+Shift+KeyN" },
  { id: "closePane", group: "tabs", combo: "Ctrl+Shift+KeyW" },
  { id: "nextTab", group: "tabs", combo: "Ctrl+Tab", repeat: true },
  { id: "prevTab", group: "tabs", combo: "Ctrl+Shift+Tab", repeat: true },
  { id: "splitRight", group: "tabs", combo: "Ctrl+Shift+KeyR" },
  { id: "splitDown", group: "tabs", combo: "Ctrl+Shift+KeyD" },
  { id: "broadcast", group: "tabs", combo: "Ctrl+Shift+KeyB" },
  { id: "toggleFiles", group: "panels", combo: "Ctrl+Shift+KeyE" },
  { id: "toggleOverview", group: "panels", combo: "Ctrl+Shift+KeyO" },
  { id: "togglePanel", group: "panels", combo: "Ctrl+Shift+KeyP" },
  { id: "snippets", group: "panels", combo: "Ctrl+Shift+KeyS" },
  { id: "searchServers", group: "panels", combo: "Ctrl+Shift+KeyL" },
  // 收起、展开左边的服务器列表：默认不设键（标签栏最左边有按钮），想用的自己设
  { id: "toggleSidebar", group: "panels", combo: null },
  { id: "goWorkspace", group: "pages", combo: null },
  { id: "goSync", group: "pages", combo: null },
  { id: "goMcp", group: "pages", combo: null },
  { id: "openSettings", group: "pages", combo: "Ctrl+Comma" },
  { id: "fontBigger", group: "font", combo: "Ctrl+Equal", repeat: true },
  { id: "fontSmaller", group: "font", combo: "Ctrl+Minus", repeat: true },
  { id: "fontReset", group: "font", combo: "Ctrl+Digit0" },
] as const satisfies readonly { id: string; group: ShortcutGroup; combo: string | null; repeat?: boolean }[];

export type ShortcutId = (typeof SHORTCUT_ACTIONS)[number]["id"];
export const SHORTCUT_GROUPS: readonly ShortcutGroup[] = ["tabs", "panels", "pages", "font"];

/** 按 ID 查动作 */
export function shortcutAction(id: ShortcutId): { id: ShortcutId; group: ShortcutGroup; combo: string | null; repeat?: boolean } {
  return SHORTCUT_ACTIONS.find((a) => a.id === id)!;
}

/** 保留键的用途：终端复制、终端粘贴、终端换行、关闭窗口、窗口菜单 */
export type ReservedUse = "copy" | "paste" | "newline" | "closeWindow" | "windowMenu";

/**
 * 保留键：Pierlink 自己在终端里用的、以及 Windows 的窗口键，不能设成快捷键。
 * Ctrl+C 有选中文字时复制、没选中时照常发给程序（中断）。
 */
export const RESERVED_KEYS: readonly { combo: string; use: ReservedUse }[] = [
  { combo: "Ctrl+KeyC", use: "copy" },
  { combo: "Ctrl+Shift+KeyC", use: "copy" },
  { combo: "Ctrl+KeyV", use: "paste" },
  { combo: "Ctrl+Shift+KeyV", use: "paste" },
  { combo: "Shift+Insert", use: "paste" },
  { combo: "Shift+Enter", use: "newline" },
  { combo: "Ctrl+Enter", use: "newline" },
  { combo: "Alt+F4", use: "closeWindow" },
  { combo: "Alt+Space", use: "windowMenu" },
];

/**
 * 这个组合键是不是保留键。
 * @param combo 规范写法
 * @returns 保留键的用途；不是保留键时为 null
 */
export function reservedUse(combo: string): ReservedUse | null {
  return RESERVED_KEYS.find((r) => r.combo === combo)?.use ?? null;
}

/** 设置里保存的：只记和默认不一样的动作（null 表示清空，不用快捷键） */
export type ShortcutOverrides = Partial<Record<ShortcutId, string | null>>;
/** 每个动作现在的快捷键（null 表示没有） */
export type ShortcutBindings = Record<ShortcutId, string | null>;

/**
 * 整理读到的快捷键设置：去掉不认识的动作、不合法的组合键、保留键、和默认一样的；
 * 两个动作改成了同一个键时只留前面那个。
 * @param raw 从设置里读到的数据（旧版本没有这一项、或被手改过）
 * @returns 合法的改动
 */
export function normalizeShortcuts(raw: unknown): ShortcutOverrides {
  const r = raw && typeof raw === "object" && !Array.isArray(raw) ? (raw as Record<string, unknown>) : {};
  const out: ShortcutOverrides = {};
  const used = new Set<string>();
  for (const a of SHORTCUT_ACTIONS) {
    if (!Object.prototype.hasOwnProperty.call(r, a.id)) continue;
    const v = r[a.id];
    if (v === null) {
      // 默认本来就没有快捷键时，清空不用记
      if (a.combo !== null) out[a.id] = null;
      continue;
    }
    const combo = normalizeCombo(v);
    if (!combo || combo === a.combo || used.has(combo) || reservedUse(combo) || comboProblem(parseCombo(combo)!)) continue;
    out[a.id] = combo;
    used.add(combo);
  }
  return out;
}

/**
 * 算出每个动作现在的快捷键。改过的优先：自己设的键不会被别的动作的默认键挤掉；
 * 默认键已经被改过的动作占了时，原来用这个默认键的动作没有快捷键。
 * @param overrides 整理过的改动（normalizeShortcuts 的结果）
 * @returns 每个动作的快捷键
 */
export function resolveBindings(overrides: ShortcutOverrides): ShortcutBindings {
  const out = {} as ShortcutBindings;
  const taken = new Set<string>();
  const assign = (id: ShortcutId, combo: string | null) => {
    out[id] = combo && !taken.has(combo) ? combo : null;
    if (out[id]) taken.add(out[id]!);
  };
  for (const a of SHORTCUT_ACTIONS) if (a.id in overrides) assign(a.id, overrides[a.id] ?? null);
  for (const a of SHORTCUT_ACTIONS) if (!(a.id in overrides)) assign(a.id, a.combo);
  return out;
}

/**
 * 改一个动作的快捷键，得到新的改动表（和默认一样时去掉这一项）。
 * @param overrides 现在的改动
 * @param id 动作
 * @param combo 新的组合键（规范写法）；null 表示清空
 * @returns 新的改动表
 */
export function withBinding(overrides: ShortcutOverrides, id: ShortcutId, combo: string | null): ShortcutOverrides {
  const out = { ...overrides };
  if (combo === shortcutAction(id).combo) delete out[id];
  else out[id] = combo;
  return out;
}

/** 录到的按键能不能用：能用时给出规范写法；不能用时给出原因（保留键给出用途，冲突给出占用它的动作） */
export type BindCheck =
  | { ok: true; combo: string }
  | { ok: false; reason: ComboProblem }
  | { ok: false; reason: "reserved"; combo: string; use: ReservedUse }
  | { ok: false; reason: "taken"; combo: string; by: ShortcutId };

/**
 * 检查录到的按键能不能设成这个动作的快捷键。
 * @param id 要改的动作
 * @param chord 录到的按键
 * @param bindings 现在每个动作的快捷键
 * @returns 检查结果
 */
export function checkBinding(id: ShortcutId, chord: Chord, bindings: ShortcutBindings): BindCheck {
  const combo = comboOf(chord);
  if (!combo) return { ok: false, reason: "modifierOnly" };
  const use = reservedUse(combo);
  if (use) return { ok: false, reason: "reserved", combo, use };
  const problem = comboProblem(chord);
  if (problem) return { ok: false, reason: problem };
  const by = SHORTCUT_ACTIONS.find((a) => a.id !== id && bindings[a.id] === combo)?.id;
  if (by) return { ok: false, reason: "taken", combo, by };
  return { ok: true, combo };
}

/**
 * 恢复默认前检查：默认键现在被别的动作占着时不能恢复。
 * @param id 要恢复的动作
 * @param bindings 现在每个动作的快捷键
 * @returns 占着默认键的动作；能恢复时为 null
 */
export function restoreBlockedBy(id: ShortcutId, bindings: ShortcutBindings): ShortcutId | null {
  const combo = shortcutAction(id).combo;
  if (!combo) return null;
  return SHORTCUT_ACTIONS.find((a) => a.id !== id && bindings[a.id] === combo)?.id ?? null;
}
