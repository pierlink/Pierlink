import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Menu, type MenuEntry } from "../../components";
import { readClipboardText } from "./clipboard";

/** 可编辑的文本框（只读、禁用的不算） */
function editableTarget(t: EventTarget | null): HTMLInputElement | HTMLTextAreaElement | null {
  if (t instanceof HTMLTextAreaElement) return t.readOnly || t.disabled ? null : t;
  if (t instanceof HTMLInputElement) {
    const textual = ["text", "search", "url", "email", "password", "number", "tel", ""].includes(t.type);
    return textual && !t.readOnly && !t.disabled ? t : null;
  }
  return null;
}

/**
 * 浏览器快捷键：在应用里没有意义，或者会造成损失（刷新页面会关掉所有终端）。
 * 在冒泡阶段拦截：终端等组件自己处理过的按键（如终端里的 Ctrl+R）不受影响。
 */
function isBrowserShortcut(e: KeyboardEvent): boolean {
  const k = e.key.toLowerCase();
  if (e.key === "F5" || e.key === "F3" || e.key === "F7" || e.key === "F12") return true;
  if (e.ctrlKey && !e.altKey) {
    if (["r", "p", "u", "j", "h", "g", "f", "s", "o", "n", "t"].includes(k) && !e.shiftKey) return true;
    if (e.shiftKey && ["r", "i", "j", "c", "delete", "n", "t"].includes(k)) return true;
  }
  if (e.altKey && (e.key === "ArrowLeft" || e.key === "ArrowRight")) return true;
  return false;
}

/**
 * 全局保护（另含：数字框的滚轮和上下方向键不改值）：
 * - 屏蔽 WebView 自带的右键菜单（返回、刷新、另存为、打印……）；文本框里换成应用自己的编辑菜单；
 * - 屏蔽浏览器快捷键（刷新、打印、查找、开发者工具等）。
 * 开发模式下按住 Shift 右键仍显示原生菜单，便于调试。
 */
export function GlobalGuards() {
  const { t } = useTranslation();
  const [menu, setMenu] = useState<{ x: number; y: number; entries: MenuEntry[] } | null>(null);

  useEffect(() => {
    const onContextMenu = (e: MouseEvent) => {
      if (import.meta.env.DEV && e.shiftKey) return;
      // 组件已经显示了自己的菜单
      if (e.defaultPrevented) return;
      e.preventDefault();
      const input = editableTarget(e.target);
      if (!input) return;
      input.focus();
      const hasSelection = (input.selectionStart ?? 0) !== (input.selectionEnd ?? 0);
      const secret = input instanceof HTMLInputElement && input.type === "password";
      setMenu({
        x: e.clientX,
        y: e.clientY,
        entries: [
          { id: "cut", label: t("common.cut"), shortcut: "Ctrl X", disabled: !hasSelection || secret, onSelect: () => { input.focus(); document.execCommand("cut"); } },
          { id: "copy", label: t("common.copy"), shortcut: "Ctrl C", disabled: !hasSelection || secret, onSelect: () => { input.focus(); document.execCommand("copy"); } },
          {
            id: "paste",
            label: t("common.paste"),
            shortcut: "Ctrl V",
            onSelect: () => {
              void readClipboardText().then((text) => {
                input.focus();
                // insertText 会触发 input 事件，受控组件（React）能收到变化，也保留撤销记录
                document.execCommand("insertText", false, text);
              });
            },
          },
          { kind: "separator", id: "s" },
          { id: "all", label: t("common.selectAll"), shortcut: "Ctrl A", onSelect: () => { input.focus(); input.select(); } },
        ],
      });
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (isBrowserShortcut(e)) e.preventDefault();
      // 数字框里上下方向键不改值
      if ((e.key === "ArrowUp" || e.key === "ArrowDown") && e.target instanceof HTMLInputElement && e.target.type === "number") e.preventDefault();
    };
    // 滚轮不改数字框的值（防止以后有人用 type="number" 时，鼠标滚过就改掉设置）
    const onWheel = (e: WheelEvent) => {
      if (e.target instanceof HTMLInputElement && e.target.type === "number") e.preventDefault();
    };
    window.addEventListener("contextmenu", onContextMenu);
    window.addEventListener("keydown", onKeyDown);
    window.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      window.removeEventListener("contextmenu", onContextMenu);
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("wheel", onWheel);
    };
  }, [t]);

  return menu ? <Menu x={menu.x} y={menu.y} entries={menu.entries} label={t("common.edit")} onClose={() => setMenu(null)} /> : null;
}
