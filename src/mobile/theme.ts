import { applyTheme } from "../theme";

/** 手机页面的深浅色：跟随电脑（默认）、跟随手机系统、固定深色或浅色 */
export type MobileThemePref = "pc" | "phone" | "dark" | "light";

/** 上次从电脑拿到的深浅色：下次打开页面先用它，不等接口返回，避免先闪一下另一种颜色 */
const PC_KEY = "pierlink.mobile.pcTheme";

function readPc(): "dark" | "light" | null {
  try {
    const v = localStorage.getItem(PC_KEY);
    return v === "dark" || v === "light" ? v : null;
  } catch {
    return null;
  }
}

let pcTheme = readPc();
let pref: MobileThemePref = "pc";

/** 按偏好和电脑的深浅色重新应用；手机系统切换深浅色时也调用 */
export function refreshTheme(): void {
  // 跟随电脑但还不知道电脑用什么时，先跟随手机
  applyTheme(pref === "pc" ? pcTheme ?? "system" : pref === "phone" ? "system" : pref);
}

/**
 * 记下电脑界面现在的深浅色（来自 /api/me）。
 * @param theme "dark" / "light"；电脑没报过时为空
 */
export function setPcTheme(theme: string | null | undefined): void {
  const next = theme === "dark" || theme === "light" ? theme : null;
  if (next === null || next === pcTheme) return;
  pcTheme = next;
  try {
    localStorage.setItem(PC_KEY, next);
  } catch {
    // 存不了就只在这次打开里有效
  }
  refreshTheme();
}

/**
 * 换偏好并立即应用。
 * @param next 新的偏好
 */
export function setThemePref(next: MobileThemePref): void {
  pref = next;
  refreshTheme();
}
