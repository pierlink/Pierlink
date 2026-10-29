import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { DesktopPane } from "./DesktopPane";
import { Tooltips } from "../shell/Tooltips";

/** 独立窗口的地址片段：#desktop-window?server=服务器ID&name=窗口标题 */
export const DESKTOP_WINDOW_HASH = "#desktop-window";

/** 当前页面是不是桌面的独立窗口 */
export function isDesktopWindow(): boolean {
  return typeof location !== "undefined" && location.hash.startsWith(DESKTOP_WINDOW_HASH);
}

/** 独立窗口的地址（相对页面） */
export function desktopWindowUrl(serverId: string, name: string): string {
  return `index.html${DESKTOP_WINDOW_HASH}?server=${encodeURIComponent(serverId)}&name=${encodeURIComponent(name)}`;
}

/** 独立窗口：只有一个桌面窗格，自动连上这台服务器上次连的桌面 */
export function DesktopWindow() {
  const { t } = useTranslation();
  const query = new URLSearchParams(location.hash.slice(DESKTOP_WINDOW_HASH.length).replace(/^\?/, ""));
  const serverId = query.get("server") ?? "";
  const name = query.get("name") ?? "";
  const title = name || t("desktop.title");
  useEffect(() => { document.title = title; }, [title]);
  return (
    <div style={{ height: "100vh", display: "flex", flexDirection: "column", background: "var(--surface-term)" }}>
      <DesktopPane serverId={serverId} visible standalone windowTitle={title} title={<span className="pane__title pl-ellipsis"><strong>{title}</strong></span>} />
      {/* 和主窗口一样的悬停提示（浏览器自带的提示点过以后要移出再移入才显示） */}
      <Tooltips />
    </div>
  );
}
