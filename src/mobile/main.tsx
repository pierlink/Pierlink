import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "../styles/base.css";
import i18n from "./i18n";
import { applyTheme } from "../theme";
import { MobileApp } from "./App";
import "./mobile.css";

// 页面语言跟随实际显示的语言（读屏、浏览器翻译据此判断）
document.documentElement.lang = i18n.language;
// 手机跟随系统的深浅色
applyTheme("system");
window.matchMedia?.("(prefers-color-scheme: dark)").addEventListener("change", () => applyTheme("system"));

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <MobileApp />
  </StrictMode>,
);
