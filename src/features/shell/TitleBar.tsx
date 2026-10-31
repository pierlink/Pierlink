import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Minus, Square, X } from "lucide-react";
import { Segmented } from "../../components";
import { useSettings } from "../settings/store";
import { TransferButton } from "../files/TransferCenter";
import { McpIndicator } from "../mcp/McpIndicator";
import "./shell.css";

/** 是否运行在 Tauri 窗口里（浏览器里调试时为 false） */
export const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

async function windowAction(action: "minimize" | "toggleMaximize" | "close") {
  if (!inTauri) return;
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow()[action]();
}

export type Page = "workspace" | "sync" | "mcp" | "settings" | "gallery";

/** 自绘标题栏：可拖动区域、产品名、主导航、窗口按钮。 */
export function TitleBar({ page, onPage, showGallery }: { page: Page; onPage: (p: Page) => void; showGallery: boolean }) {
  const { t } = useTranslation();
  const { settings } = useSettings();
  const [tray, setTray] = useState(false);
  useEffect(() => { void import("./ExitGuard").then((m) => m.trayAvailable()).then(setTray); }, []);
  const closeLabel = settings.closeToTray && tray ? t("window.close") : t("window.quit");
  const pages: { value: Page; label: string }[] = [
    { value: "workspace", label: t("nav.workspace") },
    { value: "sync", label: t("nav.sync") },
    { value: "mcp", label: t("nav.mcp") },
    { value: "settings", label: t("nav.settings") },
  ];
  if (showGallery) pages.push({ value: "gallery", label: t("nav.gallery") });

  return (
    <header data-tauri-drag-region style={{ height: "var(--titlebar-h)", display: "flex", alignItems: "center", gap: 16, padding: "0 0 0 16px", flex: "none" }}>
      <div data-tauri-drag-region style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="var(--accent)" strokeWidth="1.8" aria-hidden="true">
          <circle cx="9" cy="12" r="6" />
          <circle cx="15" cy="12" r="6" />
        </svg>
        <span style={{ fontWeight: 600 }}>{t("app.name")}</span>
      </div>
      <nav aria-label={t("app.name")}>
        <Segmented options={pages} value={page} onChange={onPage} label={t("app.name")} />
      </nav>
      <div data-tauri-drag-region style={{ flex: 1, alignSelf: "stretch" }} />
      <div className="titlebar__status">
        <McpIndicator />
        <TransferButton />
      </div>
      {/* 窗口按钮：和 Windows 自带的一样，占满标题栏高度、贴住右边缘 */}
      <div className="titlebar__caps">
        <button type="button" className="titlebar__cap" aria-label={t("window.minimize")} title={t("window.minimize")} onClick={() => void windowAction("minimize")}><Minus size={15} strokeWidth={1.5} /></button>
        <button type="button" className="titlebar__cap" aria-label={t("window.maximize")} title={t("window.maximize")} onClick={() => void windowAction("toggleMaximize")}><Square size={12} strokeWidth={1.5} /></button>
        <button type="button" className="titlebar__cap titlebar__cap--close" aria-label={closeLabel} title={closeLabel} onClick={() => void windowAction("close")}><X size={16} strokeWidth={1.5} /></button>
      </div>
    </header>
  );
}
