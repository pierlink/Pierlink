import type { ThemePref } from "../../theme";

/** 应用设置。新增字段必须给默认值（旧版本保存的设置里没有它）。 */
export interface AppSettings {
  theme: ThemePref;
  /** 界面语言："system" 表示跟随系统 */
  language: "system" | "zh-CN" | "en";
  /** 关闭窗口时缩到托盘（同步和终端继续运行）；关掉后关闭窗口 = 退出应用 */
  closeToTray: boolean;
  terminal: {
    fontSize: number;
    cursorStyle: "bar" | "block" | "underline";
    cursorBlink: boolean;
    scrollback: number;
    /** 默认本机终端 ID；null 表示按检测结果自动选择 */
    defaultShell: string | null;
    /** 粘贴多行内容前确认 */
    confirmMultilinePaste: boolean;
    /** 右键复制粘贴：有选中文字时右键复制，没有时右键粘贴；打开后右键不再弹菜单 */
    rightClickCopyPaste: boolean;
    /** 关闭正在运行的终端标签前确认 */
    confirmClose: boolean;
    /** 广播输入时，回车前确认危险命令（rm -rf、reboot 等） */
    broadcastConfirmDanger: boolean;
  };
  desktop: {
    /** 打开服务器桌面时直接用独立窗口（不在主窗口里开窗格） */
    openInWindow: boolean;
  };
  updates: {
    /** 启动后自动检查新版本 */
    autoCheck: boolean;
  };
  /** 一次性说明是否已经看过 */
  notices: {
    /** 升级到 0.4.0 后“已改用内置 SSH”的说明 */
    builtinSsh: boolean;
  };
  syncDefaults: {
    watchDelayS: number;
    conflictMode: "ask" | "newest";
    keepVersions: boolean;
    ignoreRules: string[];
  };
}

/** 设计 3.3 的默认排除规则 */
export const DEFAULT_IGNORE_RULES = [
  "(?d)node_modules", "(?d).venv", "(?d)__pycache__", "(?d)target", "(?d)dist", "(?d)build", "(?d).cache",
  "(?d)*.tmp", ".env", "*.pem", "*.key", "(?d).git", "(?d)Thumbs.db", "(?d)desktop.ini", "(?d)~$*", "(?d)*.pierlink-part",
];

export const DEFAULT_SETTINGS: AppSettings = {
  theme: "dark",
  language: "system",
  closeToTray: true,
  terminal: { fontSize: 13, cursorStyle: "bar", cursorBlink: true, scrollback: 10000, defaultShell: null, confirmMultilinePaste: true, rightClickCopyPaste: false, confirmClose: true, broadcastConfirmDanger: true },
  desktop: { openInWindow: false },
  updates: { autoCheck: true },
  notices: { builtinSsh: false },
  syncDefaults: { watchDelayS: 10, conflictMode: "ask", keepVersions: true, ignoreRules: DEFAULT_IGNORE_RULES },
};

const clamp = (v: unknown, lo: number, hi: number, d: number) =>
  typeof v === "number" && Number.isFinite(v) ? Math.min(hi, Math.max(lo, Math.round(v))) : d;
const oneOf = <T extends string>(v: unknown, opts: readonly T[], d: T): T => (opts.includes(v as T) ? (v as T) : d);
const bool = (v: unknown, d: boolean) => (typeof v === "boolean" ? v : d);

/**
 * 把读到的任意数据整理成合法设置：缺的字段用默认值，越界的数值收回范围，未知取值改回默认。
 * @param raw 从存储读出的数据（可能来自旧版本或被手改过）
 * @returns 一定合法的设置
 */
export function normalizeSettings(raw: unknown): AppSettings {
  const r = (raw && typeof raw === "object" ? raw : {}) as Record<string, any>;
  const t = (r.terminal ?? {}) as Record<string, unknown>;
  const s = (r.syncDefaults ?? {}) as Record<string, unknown>;
  const u = (r.updates ?? {}) as Record<string, unknown>;
  const dk = (r.desktop ?? {}) as Record<string, unknown>;
  const n = (r.notices ?? {}) as Record<string, unknown>;
  const d = DEFAULT_SETTINGS;
  const rules = Array.isArray(s.ignoreRules) && s.ignoreRules.every((x) => typeof x === "string") && s.ignoreRules.length <= 10000
    ? (s.ignoreRules as string[]).map((x) => x.trim()).filter((x) => x && !x.includes("\0"))
    : d.syncDefaults.ignoreRules;
  return {
    theme: oneOf(r.theme, ["dark", "light", "system"] as const, d.theme),
    language: oneOf(r.language, ["system", "zh-CN", "en"] as const, d.language),
    closeToTray: bool(r.closeToTray, d.closeToTray),
    terminal: {
      fontSize: clamp(t.fontSize, 10, 24, d.terminal.fontSize),
      cursorStyle: oneOf(t.cursorStyle, ["bar", "block", "underline"] as const, d.terminal.cursorStyle),
      cursorBlink: bool(t.cursorBlink, d.terminal.cursorBlink),
      scrollback: clamp(t.scrollback, 1000, 100000, d.terminal.scrollback),
      defaultShell: typeof t.defaultShell === "string" && t.defaultShell.length <= 200 ? t.defaultShell : null,
      confirmMultilinePaste: bool(t.confirmMultilinePaste, d.terminal.confirmMultilinePaste),
      // 0.9.2 以前是两个开关（右键直接复制、右键直接粘贴）：开过其中任何一个，就当作打开了合并后的开关
      rightClickCopyPaste: bool(t.rightClickCopyPaste, t.rightClickCopy === true || t.rightClickPaste === true || d.terminal.rightClickCopyPaste),
      broadcastConfirmDanger: bool(t.broadcastConfirmDanger, d.terminal.broadcastConfirmDanger),
      confirmClose: bool(t.confirmClose, d.terminal.confirmClose),
    },
    desktop: { openInWindow: bool(dk.openInWindow, d.desktop.openInWindow) },
    updates: { autoCheck: bool(u.autoCheck, d.updates.autoCheck) },
    notices: { builtinSsh: bool(n.builtinSsh, d.notices.builtinSsh) },
    syncDefaults: {
      watchDelayS: clamp(s.watchDelayS, 1, 60, d.syncDefaults.watchDelayS),
      conflictMode: oneOf(s.conflictMode, ["ask", "newest"] as const, d.syncDefaults.conflictMode),
      keepVersions: bool(s.keepVersions, d.syncDefaults.keepVersions),
      ignoreRules: rules,
    },
  };
}
