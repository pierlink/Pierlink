import type { ThemePref } from "../../theme";
import { normalizeShortcuts, type ShortcutOverrides } from "../shortcuts/actions";

/** 应用设置。新增字段必须给默认值（旧版本保存的设置里没有它）。 */
export interface AppSettings {
  theme: ThemePref;
  /** 界面语言："system" 表示跟随系统 */
  language: "system" | "zh-CN" | "en";
  /** 关闭窗口时缩到托盘（同步和终端继续运行）；关掉后关闭窗口 = 退出应用 */
  closeToTray: boolean;
  /** 开机自动启动（登录 Windows 后在托盘里启动，不弹窗口） */
  autostart: boolean;
  terminal: {
    fontSize: number;
    cursorStyle: "bar" | "block" | "underline";
    /** 竖线光标的粗细（像素） */
    cursorWidth: 1 | 2 | 3;
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
    /** 命令预测：输入时在光标后面用浅色显示用过的命令（服务器 bash、zsh 和本机 Git Bash） */
    commandPrediction: boolean;
    /** 本机 PowerShell 的提示音（退格删到头、补全不到时“咚”一声，是 PowerShell 自己发的）；关掉后新开的 PowerShell 不响 */
    powershellBell: boolean;
    /** 程序响铃或发通知时（比如 Codex 等你确认）怎么提醒：all = 标签上的提醒点，窗口在后台时任务栏按钮也闪；tab = 只有提醒点；off = 不提醒 */
    attention: "all" | "tab" | "off";
    /** 资源管理器右键菜单里的“在 Pierlink 中打开”：在那个目录新开本机终端 */
    explorerMenu: boolean;
    /** 新开的终端用 Windows 自带的终端引擎（系统 ConPTY），不用随安装包带的新版；随包的显示不对时让用户自己退回 */
    systemConpty: boolean;
  };
  files: {
    /** 拖动传输时遇到同名文件直接覆盖：不先统计、不弹“有同名项”的选择框 */
    overwriteOnDrop: boolean;
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
  /** 快捷键：只记和默认不一样的动作（null 为清空，不用快捷键）；默认键见 shortcuts/actions.ts */
  shortcuts: ShortcutOverrides;
}

/** 设计 3.3 的默认排除规则 */
export const DEFAULT_IGNORE_RULES = [
  "(?d)node_modules", "(?d).venv", "(?d)venv", "(?d).tox", "(?d)__pycache__", "(?d)*.pyc", "(?d)*.egg-info",
  "(?d)target", "(?d)dist", "(?d)build", "(?d).cache",
  "(?d).pytest_cache", "(?d).mypy_cache", "(?d).ruff_cache", "(?d).ipynb_checkpoints",
  "(?d).next", "(?d).nuxt", "(?d).svelte-kit", "(?d).turbo", "(?d).parcel-cache", "(?d).eslintcache", "(?d).gradle",
  "(?d).idea", "(?d).vs", "(?d)*.swp", "(?d)*.swo",
  "(?d)*.tmp", ".env", "*.pem", "*.key", "(?d).git", "(?d)Thumbs.db", "(?d)desktop.ini", "(?d).DS_Store", "(?d)~$*", "(?d)*.pierlink-part",
];

/** 以前版本的默认规则：存下的规则和其中一份完全一样（用户没改过），读取时换成现在的默认规则 */
const OLD_DEFAULT_IGNORE_RULES: readonly string[][] = [
  // 1.2.4 – 1.2.5
  ["(?d)node_modules", "(?d).venv", "(?d)__pycache__", "(?d)target", "(?d)dist", "(?d)build", "(?d).cache",
    "(?d).pytest_cache", "(?d).mypy_cache", "(?d).ruff_cache",
    "(?d)*.tmp", ".env", "*.pem", "*.key", "(?d).git", "(?d)Thumbs.db", "(?d)desktop.ini", "(?d)~$*", "(?d)*.pierlink-part"],
  // 1.2.4 以前
  ["(?d)node_modules", "(?d).venv", "(?d)__pycache__", "(?d)target", "(?d)dist", "(?d)build", "(?d).cache",
    "(?d)*.tmp", ".env", "*.pem", "*.key", "(?d).git", "(?d)Thumbs.db", "(?d)desktop.ini", "(?d)~$*", "(?d)*.pierlink-part"],
  // 加 *.pierlink-part 以前
  ["(?d)node_modules", "(?d).venv", "(?d)__pycache__", "(?d)target", "(?d)dist", "(?d)build", "(?d).cache",
    "(?d)*.tmp", ".env", "*.pem", "*.key", "(?d).git", "(?d)Thumbs.db", "(?d)desktop.ini", "(?d)~$*"],
];

/**
 * 存下的默认排除规则：和以前某版的默认规则完全一样时换成现在的，用户改过的原样保留。
 * @param rules 整理过的规则
 * @returns 要用的规则
 */
function upgradeDefaultRules(rules: string[]): string[] {
  const same = (old: readonly string[]) => old.length === rules.length && old.every((x, i) => x === rules[i]);
  return OLD_DEFAULT_IGNORE_RULES.some(same) ? [...DEFAULT_IGNORE_RULES] : rules;
}

export const DEFAULT_SETTINGS: AppSettings = {
  theme: "dark",
  language: "system",
  closeToTray: true,
  autostart: false,
  terminal: { fontSize: 13, cursorStyle: "bar", cursorWidth: 2, cursorBlink: true, scrollback: 10000, defaultShell: null, confirmMultilinePaste: true, rightClickCopyPaste: false, confirmClose: true, broadcastConfirmDanger: true, commandPrediction: true, powershellBell: true, attention: "all", explorerMenu: true, systemConpty: false },
  files: { overwriteOnDrop: false },
  desktop: { openInWindow: false },
  updates: { autoCheck: true },
  notices: { builtinSsh: false },
  syncDefaults: { watchDelayS: 10, conflictMode: "ask", keepVersions: true, ignoreRules: DEFAULT_IGNORE_RULES },
  shortcuts: {},
};

const clamp = (v: unknown, lo: number, hi: number, d: number) =>
  typeof v === "number" && Number.isFinite(v) ? Math.min(hi, Math.max(lo, Math.round(v))) : d;
const oneOf = <T extends string | number>(v: unknown, opts: readonly T[], d: T): T => (opts.includes(v as T) ? (v as T) : d);
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
  const f = (r.files ?? {}) as Record<string, unknown>;
  const n = (r.notices ?? {}) as Record<string, unknown>;
  const d = DEFAULT_SETTINGS;
  const rules = Array.isArray(s.ignoreRules) && s.ignoreRules.every((x) => typeof x === "string") && s.ignoreRules.length <= 10000
    ? upgradeDefaultRules((s.ignoreRules as string[]).map((x) => x.trim()).filter((x) => x && !x.includes("\0")))
    : d.syncDefaults.ignoreRules;
  return {
    theme: oneOf(r.theme, ["dark", "light", "system"] as const, d.theme),
    language: oneOf(r.language, ["system", "zh-CN", "en"] as const, d.language),
    closeToTray: bool(r.closeToTray, d.closeToTray),
    autostart: bool(r.autostart, d.autostart),
    terminal: {
      fontSize: clamp(t.fontSize, 10, 24, d.terminal.fontSize),
      cursorStyle: oneOf(t.cursorStyle, ["bar", "block", "underline"] as const, d.terminal.cursorStyle),
      cursorWidth: oneOf(t.cursorWidth, [1, 2, 3] as const, d.terminal.cursorWidth),
      cursorBlink: bool(t.cursorBlink, d.terminal.cursorBlink),
      scrollback: clamp(t.scrollback, 1000, 100000, d.terminal.scrollback),
      defaultShell: typeof t.defaultShell === "string" && t.defaultShell.length <= 200 ? t.defaultShell : null,
      confirmMultilinePaste: bool(t.confirmMultilinePaste, d.terminal.confirmMultilinePaste),
      // 0.9.2 以前是两个开关（右键直接复制、右键直接粘贴）：开过其中任何一个，就当作打开了合并后的开关
      rightClickCopyPaste: bool(t.rightClickCopyPaste, t.rightClickCopy === true || t.rightClickPaste === true || d.terminal.rightClickCopyPaste),
      broadcastConfirmDanger: bool(t.broadcastConfirmDanger, d.terminal.broadcastConfirmDanger),
      confirmClose: bool(t.confirmClose, d.terminal.confirmClose),
      commandPrediction: bool(t.commandPrediction, d.terminal.commandPrediction),
      powershellBell: bool(t.powershellBell, d.terminal.powershellBell),
      attention: oneOf(t.attention, ["all", "tab", "off"] as const, d.terminal.attention),
      explorerMenu: bool(t.explorerMenu, d.terminal.explorerMenu),
      systemConpty: bool(t.systemConpty, d.terminal.systemConpty),
    },
    files: { overwriteOnDrop: bool(f.overwriteOnDrop, d.files.overwriteOnDrop) },
    desktop: { openInWindow: bool(dk.openInWindow, d.desktop.openInWindow) },
    updates: { autoCheck: bool(u.autoCheck, d.updates.autoCheck) },
    notices: { builtinSsh: bool(n.builtinSsh, d.notices.builtinSsh) },
    syncDefaults: {
      watchDelayS: clamp(s.watchDelayS, 1, 60, d.syncDefaults.watchDelayS),
      conflictMode: oneOf(s.conflictMode, ["ask", "newest"] as const, d.syncDefaults.conflictMode),
      keepVersions: bool(s.keepVersions, d.syncDefaults.keepVersions),
      ignoreRules: rules,
    },
    shortcuts: normalizeShortcuts(r.shortcuts),
  };
}
