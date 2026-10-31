/**
 * 手机上的显示偏好（字号、自动换行），只存在这台手机的浏览器里。
 * 私密浏览、禁用存储时读写会失败：当成没存过，不影响使用。
 */

/** 终端字号的三档（像素） */
export const FONT_SIZES = [11, 13, 15] as const;

export interface Prefs {
  /** FONT_SIZES 里的第几档 */
  font: number;
  /** 长行自动换行（Claude Code 的大段文字在手机上更好读；表格、边框会乱，所以做成开关） */
  wrap: boolean;
  /** 看终端时把电脑上的终端临时改成手机的宽度，Claude Code 等按手机宽度重新排版（离开后恢复） */
  fit: boolean;
}

const KEY = "pierlink.mobile.prefs";
const DEFAULTS: Prefs = { font: 1, wrap: true, fit: true };

export function loadPrefs(): Prefs {
  try {
    const raw = JSON.parse(localStorage.getItem(KEY) ?? "null") as Partial<Prefs> | null;
    const font = typeof raw?.font === "number" && raw.font >= 0 && raw.font < FONT_SIZES.length ? Math.floor(raw.font) : DEFAULTS.font;
    return {
      font,
      wrap: typeof raw?.wrap === "boolean" ? raw.wrap : DEFAULTS.wrap,
      fit: typeof raw?.fit === "boolean" ? raw.fit : DEFAULTS.fit,
    };
  } catch {
    return DEFAULTS;
  }
}

export function savePrefs(p: Prefs): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(p));
  } catch {
    // 存不了就只在这次打开里有效
  }
}
