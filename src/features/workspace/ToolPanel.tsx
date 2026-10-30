import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { IconButton, Segmented, StatusDot } from "../../components";
import type { ServerView } from "./types";

export type ToolPage = "server" | "snippets";

/**
 * 工作区右侧可收起的工具面板：“端口转发”页和“片段”页（命令片段）。服务器概况在服务器终端右上角。
 * 服务器页跟随当前窗格所在的服务器；当前窗格不是服务器终端时，跟随左侧列表里选中的服务器。
 * 窗口较窄时面板浮在终端上面（样式见 workspace.css）。
 */
export function ToolPanel({ page, onPage, onClose, server, follow, serverPage, snippetsPage }: {
  page: ToolPage;
  onPage: (p: ToolPage) => void;
  onClose: () => void;
  /** 服务器页显示的服务器；没有服务器时为 null */
  server: ServerView | null;
  /** 服务器从哪里来：当前窗格 / 左侧列表选中 */
  follow: "pane" | "sidebar";
  /** 端口转发页的内容，server 不为 null 时显示 */
  serverPage: ReactNode;
  snippetsPage: ReactNode;
}) {
  const { t } = useTranslation();
  return (
    <aside className="pl-panel ws__tool" aria-label={t("panel.title")}>
      <div className="ws__tool-head">
        <Segmented
          label={t("panel.title")}
          value={page}
          onChange={onPage}
          options={[
            { value: "server", label: t("panel.server") },
            { value: "snippets", label: t("panel.snippets") },
          ]}
        />
        <IconButton label={t("panel.close")} title={t("panel.close")} onClick={onClose}><X size={14} /></IconButton>
      </div>
      {page === "server" && (
        server ? (
          <>
            <div className="ws__tool-server">
              <span className="ws__tool-name pl-ellipsis">{server.name}</span>
              <StatusDot tone={server.status === "online" ? "ok" : server.status === "connecting" ? "warn" : "neutral"} label={t(`workspace.status.${server.status}`)} />
              <span className="ws__tool-follow">{follow === "pane" ? t("panel.followPane") : t("panel.followSidebar")}</span>
            </div>
            <span className="mono ws__tool-addr pl-ellipsis">{server.address}</span>
            {serverPage}
          </>
        ) : (
          <p className="ws__tool-empty">{t("panel.noServer")}</p>
        )
      )}
      {page === "snippets" && snippetsPage}
    </aside>
  );
}
