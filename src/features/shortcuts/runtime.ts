import { useEffect, useMemo, useRef } from "react";
import { useSettings } from "../settings/store";
import { resolveBindings, shortcutAction, type ShortcutBindings, type ShortcutId } from "./actions";
import { chordFromEvent, comboOf, formatCombo } from "./keys";

/** 现在生效的快捷键：组合键（规范写法）→ 动作 */
let byCombo = new Map<string, ShortcutId>();
/** 暂停计数（录制快捷键时大于 0，整个监听不响应） */
let paused = 0;
/** 每个动作的执行函数：后注册的优先（同一个动作只有一个地方在用时就是它） */
const handlers = new Map<ShortcutId, { run: () => void }[]>();

/**
 * 设置现在生效的快捷键（设置变了时由 ShortcutHost 调用）。
 * @param bindings 每个动作的快捷键
 */
export function setActiveBindings(bindings: ShortcutBindings): void {
  const next = new Map<string, ShortcutId>();
  for (const [id, combo] of Object.entries(bindings)) if (combo) next.set(combo, id as ShortcutId);
  byCombo = next;
}

/**
 * 暂停所有快捷键（录制新的组合键时）。
 * @returns 恢复函数；调用多次只算一次
 */
export function pauseShortcuts(): () => void {
  paused++;
  let done = false;
  return () => {
    if (done) return;
    done = true;
    paused--;
  };
}

/**
 * 这个键盘事件对应哪个快捷键动作（不管焦点在哪）。终端据此不处理这些键。
 * @param e 键盘事件
 * @returns 动作；不是快捷键时为 null
 */
export function shortcutOf(e: KeyboardEvent): ShortcutId | null {
  const chord = chordFromEvent(e);
  const combo = chord && comboOf(chord);
  return (combo && byCombo.get(combo)) || null;
}

/** 能输入文字的控件（终端的隐藏输入框不算）：焦点在这里时快捷键不触发 */
const TEXT_TYPES = new Set(["text", "search", "url", "email", "password", "number", "tel", ""]);
function isTextEntry(el: Element | null): boolean {
  if (!el) return false;
  if (el instanceof HTMLTextAreaElement) return !el.classList.contains("xterm-helper-textarea") && !el.readOnly && !el.disabled;
  if (el instanceof HTMLInputElement) return TEXT_TYPES.has(el.type) && !el.readOnly && !el.disabled;
  if (el instanceof HTMLSelectElement) return true;
  return el instanceof HTMLElement && el.isContentEditable;
}

/**
 * 现在能不能触发快捷键：焦点在输入框里、开着对话框、菜单或下拉列表时不触发；
 * 焦点在远程桌面画面里时键盘全给远程桌面，也不触发。
 * @param e 键盘事件
 */
function allowedHere(e: KeyboardEvent): boolean {
  const el = e.target instanceof Element ? e.target : document.activeElement;
  if (el?.closest(".desk__screen")) return false;
  if (isTextEntry(el)) return false;
  // 菜单和下拉框展开的列表都带 pl-menu（下拉框靠收到 Tab 收起，按键不能被拦走）
  return !document.querySelector("[aria-modal='true'], [role='menu'], .pl-menu");
}

/** 有快捷键执行时调用（如收起鼠标提示：事件不再往下传，提示收不到按键） */
let onFired: (() => void) | null = null;
export function setShortcutFiredHook(fn: (() => void) | null): void {
  onFired = fn;
}

/**
 * 全局按键处理（窗口的捕获阶段）：匹配到快捷键、当前能触发、有地方执行时，执行并拦下这个键，
 * 终端、文件栏等都不会再收到它。其他按键原样放过。
 * @param e 键盘事件
 */
export function handleShortcutKey(e: KeyboardEvent): void {
  if (paused > 0 || e.defaultPrevented || e.isComposing) return;
  const id = shortcutOf(e);
  if (!id || !allowedHere(e)) return;
  const list = handlers.get(id);
  const handler = list?.[list.length - 1];
  // 当前页面没有这个动作（如在设置页按工作区的键）：只挡住 WebView 自带的反应，按键照常往下传
  if (!handler) { e.preventDefault(); return; }
  e.preventDefault();
  e.stopPropagation();
  // 按住不放：切标签、调字号连续执行，开关类只算一次
  if (e.repeat && !shortcutAction(id).repeat) return;
  onFired?.();
  handler.run();
}

/**
 * 登记一个快捷键动作的执行函数（同一个动作登记了多个时，后登记的执行）。
 * @param id 动作
 * @param run 执行函数
 * @returns 取消登记的函数
 */
export function registerShortcut(id: ShortcutId, run: () => void): () => void {
  const entry = { run };
  handlers.set(id, [...(handlers.get(id) ?? []), entry]);
  return () => {
    const left = (handlers.get(id) ?? []).filter((x) => x !== entry);
    if (left.length) handlers.set(id, left);
    else handlers.delete(id);
  };
}

/**
 * 在组件里登记一个快捷键动作的执行函数，组件卸载时取消。
 * @param id 动作
 * @param run 执行函数（总是用最新的一份，不用包 useCallback）
 * @param enabled 为 false 时不登记（例如工作区没显示时，工作区的动作不响应）
 */
export function useShortcut(id: ShortcutId, run: () => void, enabled = true): void {
  const ref = useRef(run);
  ref.current = run;
  useEffect(() => (enabled ? registerShortcut(id, () => ref.current()) : undefined), [id, enabled]);
}

/** 每个动作现在的快捷键（跟着设置变） */
export function useShortcutBindings(): ShortcutBindings {
  const { settings } = useSettings();
  return useMemo(() => resolveBindings(settings.shortcuts), [settings.shortcuts]);
}

/**
 * 某个动作现在的快捷键在界面上的写法（用在按钮提示、菜单里）。
 * @param id 动作
 * @param sep 连接符（菜单里用空格）
 * @returns 如 "Ctrl+Shift+E"；没有快捷键时为空字符串
 */
export function useShortcutText(id: ShortcutId, sep = "+"): string {
  return formatCombo(useShortcutBindings()[id], sep);
}

/**
 * 按钮提示后面带上快捷键，如“工具面板（Ctrl+Shift+P）”。
 * @param t 翻译函数
 * @param label 原来的提示
 * @param key 快捷键的写法（formatCombo 的结果）；为空时原样返回
 */
export function withShortcut(t: (key: string, options?: Record<string, unknown>) => string, label: string, key: string): string {
  return key ? t("common.withNote", { text: label, note: key }) : label;
}
