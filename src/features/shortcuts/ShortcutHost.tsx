import { useEffect } from "react";
import { DEFAULT_SETTINGS } from "../settings/model";
import { useSettings } from "../settings/store";
import { hideTooltip } from "../shell/Tooltips";
import type { Page } from "../shell/TitleBar";
import { handleShortcutKey, setActiveBindings, setShortcutFiredHook, useShortcut, useShortcutBindings } from "./runtime";

/**
 * 快捷键的总开关：在窗口的捕获阶段监听按键（焦点在终端里时也先到这里），
 * 并执行不属于某个页面的动作：切页面、调终端字号。工作区的动作由工作区自己注册。
 * 只在主窗口里挂；服务器桌面的独立窗口没有快捷键，键盘全给远程桌面。
 */
export function ShortcutHost({ onPage }: { onPage: (page: Page) => void }) {
  const { update } = useSettings();
  const bindings = useShortcutBindings();
  useEffect(() => { setActiveBindings(bindings); }, [bindings]);

  useEffect(() => {
    window.addEventListener("keydown", handleShortcutKey, true);
    setShortcutFiredHook(hideTooltip);
    return () => {
      window.removeEventListener("keydown", handleShortcutKey, true);
      setShortcutFiredHook(null);
    };
  }, []);

  useShortcut("goWorkspace", () => onPage("workspace"));
  useShortcut("goSync", () => onPage("sync"));
  useShortcut("goMcp", () => onPage("mcp"));
  useShortcut("openSettings", () => onPage("settings"));
  // 字号超出范围时由设置的整理收回（10～24）
  const font = (next: (size: number) => number) => update((s) => ({ ...s, terminal: { ...s.terminal, fontSize: next(s.terminal.fontSize) } }));
  useShortcut("fontBigger", () => font((x) => x + 1));
  useShortcut("fontSmaller", () => font((x) => x - 1));
  useShortcut("fontReset", () => font(() => DEFAULT_SETTINGS.terminal.fontSize));
  return null;
}
