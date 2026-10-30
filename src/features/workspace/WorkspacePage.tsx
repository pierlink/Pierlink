import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import "./workspace.css";
import { ServerSidebar } from "./ServerSidebar";
import { TerminalArea, type OpenRequest, type SnippetRequest } from "../terminal/TerminalArea";
import { SnippetsPage } from "../snippets/SnippetsPage";
import { ForwardsSection } from "../forwards/ForwardsSection";
import { ToolPanel, type ToolPage } from "./ToolPanel";
import { StatusBar } from "./StatusBar";
import type { ServerView } from "./types";
import { Banner, Button, Dialog, Notice } from "../../components";
import { formatError, isAppError } from "../../errors";
import { inTauri } from "../shell/TitleBar";
import { serverAddress, type ServerRecord } from "../servers/api";
import { ServerDialog } from "../servers/ServerDialog";
import { useServers } from "../servers/useServers";
import { DeployFlow } from "../sync/DeployFlow";
import { ImportDialog } from "../servers/ImportDialog";
import { BatchAddDialog } from "../servers/BatchAddDialog";
import { LinkServerDialog } from "../servers/LinkServerDialog";
import { DedicatedKeyDialog } from "../servers/DedicatedKeyDialog";
import { PasswordHost } from "../passwords/PasswordHost";
import { ServerExportDialog } from "../servers/ServerExportDialog";
import { ImportReportDialog } from "../settings/ImportReportDialog";
import { importServers, type ImportResult } from "../settings/profileApi";
import { writeClipboardText } from "../shell/clipboard";

type Modal =
  | { kind: "edit"; server: ServerRecord | null }
  | { kind: "delete"; server: ServerRecord; error: { title: string; text: string } | null; busy: boolean }
  | { kind: "error"; title: string; text: string }
  | { kind: "deploy"; server: ServerRecord }
  | { kind: "import" }
  | { kind: "batch" }
  | { kind: "export"; initial: string | null }
  | { kind: "report"; result: ImportResult }
  | { kind: "key"; server: ServerRecord };

function toView(r: ServerRecord): ServerView {
  // 连接状态与同步状态在后续阶段接入；目前只区分有无打开的终端由终端区负责
  return { id: r.id, name: r.name, address: serverAddress(r), group: r.groupName, favorite: r.favorite, status: "offline", openTabs: 0, linked: r.source === "ssh_config", syncState: "none" };
}

/** 删除失败的原因：仍被同步任务使用时列出任务名 */
function deleteError(t: (k: string, o?: Record<string, unknown>) => string, e: unknown): { title: string; text: string } {
  if (isAppError(e) && e.code === "storage.server_in_use") {
    let tasks: string[] = [];
    try { tasks = JSON.parse(e.params.task_names ?? "[]") as string[]; } catch { /* 解析失败时只显示通用提示 */ }
    return { title: t("servers.inUseTitle"), text: t("servers.inUseBody", { tasks: tasks.join(t("common.listSep")) }) };
  }
  return { title: t("servers.deleteFailed"), text: formatError(e) };
}

const PANEL_KEY = "pierlink.toolPanel";

/** 读出上次右侧面板的开关和页面（默认收起）；存储不可用时用默认值 */
function loadPanel(): { open: boolean; page: ToolPage } {
  try {
    const v = JSON.parse(localStorage.getItem(PANEL_KEY) ?? "null") as { open?: unknown; page?: unknown } | null;
    return { open: v?.open === true, page: v?.page === "snippets" ? "snippets" : "server" };
  } catch {
    return { open: false, page: "server" };
  }
}

/** 工作区：服务器列表 | 终端标签区 | 工具面板，底部状态栏。 */
export function WorkspacePage() {
  const { t } = useTranslation();
  const { servers: records, loaded: serversLoaded, loadError, reload, save, remove, setFavorite } = useServers();
  const [connections, setConnections] = useState<Record<string, "online" | "connecting">>({});
  const views = useMemo(() => records.map((r) => ({ ...toView(r), status: connections[r.id] ?? "offline" })), [records, connections]);
  const groups = useMemo(() => [...new Set(records.map((r) => r.groupName).filter((g): g is string => !!g))].sort(), [records]);
  const [selected, setSelected] = useState<string | null>(null);
  // 开发预览：#workspace-import 直接打开导入对话框
  const [modal, setModal] = useState<Modal | null>(() => (import.meta.env.DEV && window.location.hash === "#workspace-import" ? { kind: "import" } : null));
  const [openRequest, setOpenRequest] = useState<OpenRequest | null>(null);
  const [copied, setCopied] = useState(false);
  // 右侧工具面板：开关和页面记在本机（每台电脑自己的习惯）
  const [panel, setPanel] = useState(loadPanel);
  useEffect(() => {
    try { localStorage.setItem(PANEL_KEY, JSON.stringify(panel)); } catch { /* 存不了就算了 */ }
  }, [panel]);
  // 当前窗格所在的服务器（面板优先跟随它）
  const [paneServer, setPaneServer] = useState<string | null>(null);
  // 命令片段：发送请求交给终端区；终端区报告现在会发到几个窗格
  const [snippetRequest, setSnippetRequest] = useState<SnippetRequest | null>(null);
  const [snippetTargets, setSnippetTargets] = useState(0);

  // 开发预览：#workspace-link 打开第一台导入服务器的设置
  const linkDemo = useRef(false);
  useEffect(() => {
    if (!import.meta.env.DEV || linkDemo.current) return;
    const h = window.location.hash;
    // #workspace-key 打开第一台服务器的专用密钥对话框
    if (h === "#workspace-key" && records[0]) { linkDemo.current = true; setModal({ kind: "key", server: records[0] }); return; }
    if (h !== "#workspace-link") return;
    const s = records.find((r) => r.source === "ssh_config");
    if (s) { linkDemo.current = true; setModal({ kind: "edit", server: s }); }
  }, [records]);

  const server = views.find((s) => s.id === selected) ?? views[0] ?? null;
  const paneView = paneServer ? views.find((s) => s.id === paneServer) ?? null : null;
  // 面板只跟随当前窗格或明确选中的服务器：不退到列表第一台（否则一打开面板就会去登录一台没选的服务器）
  const panelServer = paneView ?? (selected ? views.find((s) => s.id === selected) ?? null : null);
  const byId = (id: string) => records.find((r) => r.id === id);
  const fail = (title: string) => (e: unknown) => setModal({ kind: "error", title, text: formatError(e) });

  return (
    <div className="ws">
      <div className="ws__body">
        <ServerSidebar
          servers={views}
          selectedId={server?.id ?? null}
          onSelect={setSelected}
          actions={{
            onAdd: () => setModal({ kind: "edit", server: null }),
            onImport: () => setModal({ kind: "import" }),
            onBatchAdd: () => setModal({ kind: "batch" }),
            onImportFile: () => {
              importServers().then((r) => {
                if (!r) return;
                void reload();
                setModal({ kind: "report", result: r });
              }, fail(t("workspace.importFileFailed")));
            },
            onExport: (id) => setModal({ kind: "export", initial: id ?? null }),
            onOpen: (id, mode = "tab") => setOpenRequest((r) => ({ id, mode, n: (r?.n ?? 0) + 1 })),
            onDedicatedKey: (id) => { const s = byId(id); if (s) setModal({ kind: "key", server: s }); },
            onEdit: (id) => { const s = byId(id); if (s) setModal({ kind: "edit", server: s }); },
            onDelete: (id) => { const s = byId(id); if (s) setModal({ kind: "delete", server: s, error: null, busy: false }); },
            onToggleFavorite: (id) => { const s = byId(id); if (s) setFavorite(id, !s.favorite).catch(fail(t("servers.saveFailed"))); },
            onDeploy: (id) => { const s = byId(id); if (s) setModal({ kind: "deploy", server: s }); },
            onCopyAddress: (id) => {
              const s = byId(id);
              if (s) void writeClipboardText(serverAddress(s)).then(() => { setCopied(true); setTimeout(() => setCopied(false), 1500); });
            },
          }}
        />
        <TerminalArea
          servers={views}
          serversLoaded={serversLoaded}
          openRequest={openRequest}
          onConnections={setConnections}
          onActiveServer={setPaneServer}
          panelOpen={panel.open}
          onTogglePanel={() => setPanel((x) => ({ ...x, open: !x.open }))}
          snippetRequest={snippetRequest}
          onSnippetTargets={setSnippetTargets}
        />
        {panel.open && (
          <ToolPanel
            page={panel.page}
            onPage={(page) => setPanel((x) => ({ ...x, page }))}
            onClose={() => setPanel((x) => ({ ...x, open: false }))}
            server={panelServer}
            follow={paneView ? "pane" : "sidebar"}
            serverPage={panelServer ? <ForwardsSection serverId={panelServer.id} /> : null}
            snippetsPage={(
              <SnippetsPage
                group={panelServer?.group ?? null}
                groups={groups}
                targets={snippetTargets}
                onSend={(text, execute) => setSnippetRequest((r) => ({ text, execute, n: (r?.n ?? 0) + 1 }))}
              />
            )}
          />
        )}
      </div>
      {!inTauri && <StatusBar tunnelsUp={2} tunnelsTotal={2} conflicts={1} />}
      {copied && <div className="ws-toast" role="status">{t("servers.copied")}</div>}
      {loadError !== null && (
        <Notice order={0}><div className="ws-load-error"><Banner tone="danger" title={t("servers.loadFailed")}>{formatError(loadError)}</Banner></div></Notice>
      )}

      {modal?.kind === "edit" && modal.server?.source === "ssh_config" && (
        <LinkServerDialog server={modal.server} groups={groups} onSaved={() => void reload()} onClose={() => setModal(null)} />
      )}
      {modal?.kind === "edit" && modal.server?.source !== "ssh_config" && (
        <ServerDialog server={modal.server} groups={groups} onSave={async (input) => { const s = await save(input); setSelected(s.id); }} onClose={() => setModal(null)} />
      )}
      {modal?.kind === "delete" && (
        <Dialog
          title={t("servers.deleteTitle", { name: modal.server.name })}
          danger
          onDismiss={modal.busy ? undefined : () => setModal(null)}
          actions={<>
            <Button onClick={() => setModal(null)} disabled={modal.busy}>{t("common.cancel")}</Button>
            <Button
              variant="danger"
              disabled={modal.busy || modal.error !== null}
              onClick={() => {
                const m = modal;
                setModal({ ...m, busy: true });
                remove(m.server.id).then(() => setModal(null), (e: unknown) => setModal({ ...m, busy: false, error: deleteError(t, e) }));
              }}
            >
              {t("common.delete")}
            </Button>
          </>}
        >
          {t("servers.deleteBody")}
          {modal.error && <div style={{ marginTop: 10 }}><Banner tone="danger" title={modal.error.title}>{modal.error.text}</Banner></div>}
        </Dialog>
      )}
      {/* ssh 要密码时的全局密码框（输一次，所有功能共用） */}
      <PasswordHost servers={records} onDedicatedKey={(id) => { const s = byId(id); if (s) setModal({ kind: "key", server: s }); }} />
      {modal?.kind === "key" && <DedicatedKeyDialog server={modal.server} onChanged={() => void reload()} onClose={() => setModal(null)} />}
      {modal?.kind === "batch" && <BatchAddDialog existing={records} onAdded={() => void reload()} onClose={() => setModal(null)} />}
      {modal?.kind === "export" && <ServerExportDialog servers={records} initial={modal.initial} onClose={() => setModal(null)} />}
      {modal?.kind === "report" && <ImportReportDialog result={modal.result} serversOnly onClose={() => setModal(null)} />}
      {modal?.kind === "import" && <ImportDialog onImported={() => void reload()} onClose={() => setModal(null)} />}
      {modal?.kind === "deploy" && <DeployFlow serverId={modal.server.id} serverName={modal.server.name} onClose={() => setModal(null)} />}
      {modal?.kind === "error" && (
        <Dialog title={modal.title} danger onDismiss={() => setModal(null)} actions={<Button variant="primary" onClick={() => setModal(null)}>{t("common.ok")}</Button>}>
          {modal.text}
        </Dialog>
      )}
    </div>
  );
}
