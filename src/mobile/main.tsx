import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "../styles/base.css";
import i18n from "./i18n";
import { MobileApp } from "./App";
import { loadPrefs } from "./prefs";
import { refreshTheme, setThemePref } from "./theme";
import "./mobile.css";

// 页面语言跟随实际显示的语言（读屏、浏览器翻译据此判断）
document.documentElement.lang = i18n.language;
// 深浅色默认跟随电脑（还不知道电脑用什么时先跟随手机）；手机系统切换时按偏好重新应用
setThemePref(loadPrefs().theme);
window.matchMedia?.("(prefers-color-scheme: dark)").addEventListener("change", refreshTheme);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <MobileApp />
  </StrictMode>,
);
