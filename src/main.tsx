import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./styles/fonts.css";
import "./styles/base.css";
import "./i18n";
import App from "./App";
import { DesktopWindow, isDesktopWindow } from "./features/desktop/DesktopWindow";
import { SettingsProvider } from "./features/settings/store";
// 交互反馈样式放在所有组件样式之后，才能叠加在它们之上
import "./styles/interaction.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <SettingsProvider>
      {/* 桌面的独立窗口只放一个桌面窗格，不带主窗口的标题栏和全局提示 */}
      {isDesktopWindow() ? <DesktopWindow /> : <App />}
    </SettingsProvider>
  </StrictMode>,
);
