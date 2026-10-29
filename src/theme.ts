/** 主题偏好：深色、浅色或跟随系统 */
export type ThemePref = "dark" | "light" | "system";

/**
 * 把主题偏好应用到页面根元素上。
 * @param pref 用户选择的主题偏好
 * @returns 实际生效的主题
 */
export function applyTheme(pref: ThemePref): "dark" | "light" {
  const systemDark = window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? true;
  const actual = pref === "system" ? (systemDark ? "dark" : "light") : pref;
  document.documentElement.dataset.theme = actual;
  return actual;
}
