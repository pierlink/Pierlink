import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowDownUp, ChevronDown, Monitor, Plus, RefreshCw, Search } from "lucide-react";
import { IconButton, Menu, Segmented, StatusDot, useContextMenu, type MenuEntry } from "../../components";
import type { ServerView } from "./types";
import { BuiltinSshNotice } from "./BuiltinSshNotice";
import { focusDesktopWindow, useDesktopPopouts } from "../desktop/popouts";
import { useSettings } from "../settings/store";

type Filter = "all" | "favorites" | "online";

const dotTone = { online: "ok", connecting: "warn", offline: "neutral" } as const;

/** 服务器列表上的操作（由工作区页面实现） */
export interface ServerActions {
  onAdd: () => void;
  onImport: () => void;
  onBatchAdd: () => void;
  /** 从别人导出的文件导入服务器 */
  onImportFile: () => void;
  /** 导出服务器（id 有值时默认只勾这一台） */
  onExport: (id?: string) => void;
  /** 打开终端：新标签、在当前窗格右边分屏、或新标签并打开文件列表 */
  onOpen: (id: string, mode?: "tab" | "split" | "desktop") => void;
  onEdit: (id: string) => void;
  onDelete: (id: string) => void;
  onToggleFavorite: (id: string) => void;
  /** 打开或关闭这台服务器的命令预测 */
  onTogglePrediction: (id: string) => void;
  onCopyAddress: (id: string) => void;
  onDeploy: (id: string) => void;
  onDedicatedKey: (id: string) => void;
}

/** 左侧服务器列表：搜索、筛选、分组、右键菜单。双击或回车打开终端。 */
export function ServerSidebar({ servers, selectedId, onSelect, actions }: { servers: ServerView[]; selectedId: string | null; onSelect: (id: string) => void; actions: ServerActions }) {
  const { t } = useTranslation();
  const { settings } = useSettings();
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<Filter>("all");
  const ctx = useContextMenu(t("workspace.servers"));
  const popouts = useDesktopPopouts();
  const [importMenu, setImportMenu] = useState<{ x: number; y: number } | null>(null);

  const groups = useMemo(() => {
    const q = query.trim().toLowerCase();
    const visible = servers.filter((s) => {
      if (filter === "favorites" && !s.favorite) return false;
      if (filter === "online" && s.status !== "online") return false;
      return !q || [s.name, s.address, s.group ?? ""].some((x) => x.toLowerCase().includes(q));
    });
    const map = new Map<string, ServerView[]>();
    for (const s of visible) {
      const key = s.favorite ? t("workspace.filterFavorites") : (s.group ?? t("workspace.servers"));
      map.set(key, [...(map.get(key) ?? []), s]);
    }
    return [...map.entries()];
  }, [servers, query, filter, t]);

  // 还没实现的功能显示为灰色（不可点），不放假按钮
  const menuFor = (s: ServerView): MenuEntry[] => [
    { id: "open", label: t("menu.openTerminal"), shortcut: "Enter", onSelect: () => actions.onOpen(s.id) },
    { id: "split", label: t("menu.openInSplit"), onSelect: () => actions.onOpen(s.id, "split") },
    { id: "desktop", label: t("menu.openDesktop"), onSelect: () => actions.onOpen(s.id, "desktop") },
    { kind: "separator", id: "s1" },
    { id: "edit", label: t("menu.edit"), shortcut: "F2", onSelect: () => actions.onEdit(s.id) },
    { id: "copyAddr", label: t("menu.copyAddress"), onSelect: () => actions.onCopyAddress(s.id) },
    { kind: "separator", id: "s2" },
    { id: "deploy", label: t("menu.deploySync"), onSelect: () => actions.onDeploy(s.id) },
    // 导入自 ssh config 的服务器由用户在 ssh config 里管理密钥
    ...(s.linked ? [] : [{ id: "key", label: t("menu.dedicatedKey"), onSelect: () => actions.onDedicatedKey(s.id) }]),
    { id: "fav", label: s.favorite ? t("menu.unfavorite") : t("menu.favorite"), onSelect: () => actions.onToggleFavorite(s.id) },
    // 全局的命令预测开着时才有意义：可以只对这台服务器关掉
    ...(settings.terminal.commandPrediction
      ? [{ id: "predict", label: s.noPrediction ? t("menu.predictionOn") : t("menu.predictionOff"), onSelect: () => actions.onTogglePrediction(s.id) }]
      : []),
    ...(s.linked ? [] : [{ id: "export", label: t("menu.exportServer"), onSelect: () => actions.onExport(s.id) }]),
    { kind: "separator", id: "s3" },
    { id: "delete", label: t("menu.deleteServer"), danger: true, onSelect: () => actions.onDelete(s.id) },
  ];

  return (
    <nav aria-label={t("workspace.servers")} className="pl-panel ws__side">
      <div style={{ display: "flex", alignItems: "center", padding: "0 4px" }}>
        <span className="ws__label" style={{ flex: 1 }}>{t("workspace.servers")}</span>
        <IconButton
          size="sm"
          label={t("workspace.importMenu")}
          onClick={(e) => {
            const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
            setImportMenu({ x: r.left, y: r.bottom + 4 });
          }}
        >
          <ArrowDownUp size={14} />
        </IconButton>
        <IconButton size="sm" label={t("workspace.addServer")} onClick={actions.onAdd}><Plus size={14} /></IconButton>
      </div>
      <BuiltinSshNotice />
      <label className="srv-search">
        <Search size={13} aria-hidden="true" />
        <span className="visually-hidden">{t("common.search")}</span>
        <input type="search" placeholder={t("workspace.searchServers")} value={query} onChange={(e) => setQuery(e.target.value)} />
      </label>
      <Segmented
        label={t("workspace.servers")}
        value={filter}
        onChange={setFilter}
        options={[
          { value: "all", label: t("workspace.filterAll", { count: servers.length }) },
          { value: "favorites", label: t("workspace.filterFavorites") },
          { value: "online", label: t("workspace.filterOnline") },
        ]}
      />
      {groups.length === 0 && servers.length > 0 && (
        <span className="srv-filter-empty">{filter === "online" ? t("workspace.emptyOnline") : filter === "favorites" ? t("workspace.emptyFavorites") : t("workspace.emptySearch")}</span>
      )}
      {groups.map(([name, items]) => (
        <div key={name} className="srv-group">
          <div className="srv-group__head"><ChevronDown size={10} aria-hidden="true" /><span style={{ flex: 1 }}>{name}</span><span>{items.length}</span></div>
          {items.map((s) => (
            <button
              key={s.id}
              type="button"
              className="srv-row"
              aria-current={s.id === selectedId}
              onClick={() => onSelect(s.id)}
              onDoubleClick={() => actions.onOpen(s.id)}
              onKeyDown={(e) => {
                if (e.key === "Enter") { e.preventDefault(); actions.onOpen(s.id); }
                if (e.key === "F2") { e.preventDefault(); actions.onEdit(s.id); }
                if (e.key === "Delete") { e.preventDefault(); actions.onDelete(s.id); }
              }}
              onContextMenu={(e) => { onSelect(s.id); ctx.open(e, menuFor(s)); }}
            >
              <StatusDot tone={dotTone[s.status]} label={t(`workspace.status.${s.status}`)} />
              <span className="srv-row__text">
                <span className="srv-row__name pl-ellipsis">{s.name}</span>
                <span className="srv-row__addr pl-ellipsis">{s.address}</span>
              </span>
              {s.syncState !== "none" && (
                <RefreshCw size={13} role="img" aria-label={t("workspace.hasSync")} color={s.syncState === "warn" ? "var(--warn)" : s.syncState === "busy" ? "var(--accent)" : "var(--ok)"} />
              )}
              {popouts.has(s.id) && (
                // 桌面开在独立窗口里：点一下把那个窗口提到前面
                <span className="srv-row__desk" role="img" title={t("workspace.desktopInWindow")} aria-label={t("workspace.desktopInWindow")}
                  onClick={(e) => { e.stopPropagation(); void focusDesktopWindow(s.id); }}>
                  <Monitor size={13} aria-hidden="true" />
                </span>
              )}
              {s.openTabs > 0 && <span className="srv-row__count" title={t("workspace.openTabs", { count: s.openTabs })}>{s.openTabs}</span>}
            </button>
          ))}
        </div>
      ))}
      {servers.length === 0 && (
        <div className="srv-empty">
          <b>{t("servers.empty")}</b>
          <span>{t("servers.emptyHint")}</span>
          <button type="button" className="pl-btn pl-btn--primary" onClick={actions.onAdd}><Plus size={14} />{t("workspace.addServer")}</button>
        </div>
      )}
      <div style={{ flex: 1 }} />
      {ctx.node}
      {importMenu && (
        <Menu
          x={importMenu.x}
          y={importMenu.y}
          label={t("workspace.importMenu")}
          onClose={() => setImportMenu(null)}
          entries={[
            { id: "batch", label: t("workspace.importBatch"), onSelect: actions.onBatchAdd },
            { id: "sshconfig", label: t("workspace.importSshConfigMenu"), onSelect: actions.onImport },
            { id: "file", label: t("workspace.importFile"), onSelect: actions.onImportFile },
            { kind: "separator", id: "s1" },
            { id: "export", label: t("workspace.exportServers"), onSelect: () => actions.onExport() },
          ]}
        />
      )}
    </nav>
  );
}
