import { useEffect, useState } from "react";
import { screenshotMode } from "./dev/screenshots";
import { useTranslation } from "react-i18next";
import { TitleBar, inTauri, type Page } from "./features/shell/TitleBar";
import { ComponentGallery } from "./dev/ComponentGallery";
import { WorkspacePage } from "./features/workspace/WorkspacePage";
import { DeployDemo } from "./dev/DeployDemo";
import { NewSyncDemo } from "./dev/NewSyncDemo";
import { UninstallDemo } from "./dev/UninstallDemo";
import { ExitDemo } from "./dev/ExitDemo";
import { SyncScreen } from "./features/sync/SyncScreen";
import { SettingsPage } from "./features/settings/SettingsPage";
import { useAppVersion } from "./features/shell/useAppVersion";
import { GlobalGuards } from "./features/shell/GlobalGuards";
import { ExitGuard } from "./features/shell/ExitGuard";
import { DbRestoredNotice } from "./features/shell/DbRestoredNotice";
import { Tooltips } from "./features/shell/Tooltips";
import { NetworkBanner } from "./features/shell/NetworkBanner";
import { UpdateNotice } from "./features/updates/UpdateNotice";
import { McpConfirmHost } from "./features/mcp/McpConfirmHost";
import { PairRequestHost } from "./features/mobile/PairRequestHost";
import { McpPage } from "./features/mcp/McpPage";

/** 应用外壳：标题栏 + 当前页面。各页面在后续阶段实现。 */
export default function App() {
  const { t } = useTranslation();
  const version = useAppVersion();
  const showGallery = import.meta.env.DEV && !screenshotMode;
  // 开发时可用网址片段直接打开某页，如 #workspace
  const fromHash = window.location.hash.slice(1).split("-")[0] as Page;
  const pages: Page[] = ["workspace", "sync", "mcp", "settings", "gallery"];
  const [page, setPage] = useState<Page>(pages.includes(fromHash) ? fromHash : showGallery ? "gallery" : "workspace");
  const [settingsSeen, setSettingsSeen] = useState(page === "settings");
  if (page === "settings" && !settingsSeen) setSettingsSeen(true);

  // 托盘菜单“打开同步页”（与 Rust 端 app_shell::OPEN_PAGE_EVENT 一致）
  useEffect(() => {
    if (!inTauri) return;
    let alive = true;
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/event")
      .then(({ listen }) => listen<string>("pierlink://open-page", (e) => { if (pages.includes(e.payload as Page)) setPage(e.payload as Page); }))
      .then((u) => { if (alive) unlisten = u; else u(); });
    return () => { alive = false; unlisten?.(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div style={{ height: "100%", display: "flex", flexDirection: "column" }}>
      <GlobalGuards />
      <ExitGuard />
      <DbRestoredNotice />
      <Tooltips />
      <NetworkBanner />
      <UpdateNotice />
      <McpConfirmHost />
      <PairRequestHost />
      <TitleBar page={page} onPage={setPage} showGallery={showGallery} />
      <main style={{ flex: 1, minHeight: 0, padding: "0 8px 8px", display: "flex" }}>
        {/* 工作区一直保留（只隐藏），切到其他页面再回来时终端会话和标签都还在 */}
        <div style={{ flex: 1, minWidth: 0, display: page === "workspace" ? "flex" : "none" }}>
          <WorkspacePage />
        </div>
        {/* 设置页打开过一次后也一直保留（只隐藏），没保存的修改（如排除规则草稿）切页面不会丢 */}
        {settingsSeen && (
          <div style={{ flex: 1, minWidth: 0, display: page === "settings" ? "flex" : "none" }}>
            <SettingsPage version={version} />
          </div>
        )}
        {page === "workspace" || page === "settings" ? null : page === "gallery" ? (
          <ComponentGallery />
        ) : page === "sync" ? (
          // 后端接入前：浏览器预览用示例数据，应用里先显示空列表
          <SyncScreen />
        ) : page === "mcp" ? (
          <McpPage />
        ) : (
          <section className="pl-panel" style={{ flex: 1, display: "grid", placeItems: "center", color: "var(--text-muted)" }}>
            <div style={{ textAlign: "center" }}>
              <div style={{ fontSize: "var(--fs-xl)", color: "var(--text)" }}>{t("placeholder.title")}</div>
              <div>{t("placeholder.body")}</div>
            </div>
          </section>
        )}
      </main>
      {/* 开发预览：#sync-deploy-<场景>，场景为 review / running / failed / blocked / done */}
      {import.meta.env.DEV && window.location.hash.startsWith("#sync-deploy") && (
        <DeployDemo scenario={(window.location.hash.split("-")[2] as "review") || "review"} />
      )}
      {/* 开发预览：#sync-new-<场景> */}
      {import.meta.env.DEV && window.location.hash.startsWith("#sync-new") && <NewSyncDemo scenario={window.location.hash.split("-")[2] ?? "paths"} />}
      {import.meta.env.DEV && window.location.hash === "#sync-uninstall" && <UninstallDemo />}
      {/* 开发预览：#exit-<场景> */}
      {import.meta.env.DEV && window.location.hash.startsWith("#exit") && <ExitDemo scenario={window.location.hash.split("-")[1] ?? "unsynced"} />}
    </div>
  );
}
