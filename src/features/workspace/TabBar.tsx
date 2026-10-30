import { useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, Columns2, PanelRight, Plus, Radio, SquareTerminal, X } from "lucide-react";
import { IconButton, Menu, StatusDot, type MenuEntry } from "../../components";
import { localShellName } from "../terminal/backend";
import type { ServerView, TabView } from "./types";

const dotTone = { online: "ok", connecting: "warn", offline: "neutral" } as const;

/** 终端标签栏：标签、新建（默认终端 + 下拉选择服务器或本机终端）、分屏。 */
export function TabBar({ tabs, activeId, servers, localShells, onActivate, onClose, onNewDefault, onPickServer, onPickShell, onQuickConnect, onSplit, onTabPointerDown, panelOpen, onTogglePanel, broadcast }: {
  tabs: TabView[];
  activeId: string;
  servers: ServerView[];
  localShells: { id: string; name: string; isDefault: boolean }[];
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  /** “+”主按钮：打开默认本机终端 */
  onNewDefault: () => void;
  onPickServer: (serverId: string) => void;
  onPickShell: (shellId: string) => void;
  onQuickConnect: () => void;
  /** 分屏按钮：在当前窗格右边再开一个终端（el 为按钮，用来定位菜单） */
  onSplit: (el: HTMLElement) => void;
  /** 右侧工具面板是否打开；不传 onTogglePanel 时不显示面板按钮 */
  panelOpen?: boolean;
  onTogglePanel?: () => void;
  /** 在标签上按下鼠标（拖动标签：并入别的标签的窗格、调整顺序） */
  onTabPointerDown?: (e: ReactPointerEvent, id: string) => void;
  /** 当前标签在广播输入：在标签栏右边显示一个小标记（不另占一行，开关广播时终端区不跳动） */
  broadcast?: { count: number; hint: string; onStop: () => void };
}) {
  const { t } = useTranslation();
  const moreRef = useRef<HTMLButtonElement>(null);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);

  const entries: MenuEntry[] = [
    { kind: "header", id: "h1", label: t("workspace.serversHeader") },
    ...servers.map((s) => ({ id: `srv-${s.id}`, label: `${s.name}  ·  ${s.address}`, onSelect: () => onPickServer(s.id) })),
    { id: "quick", label: `${t("terminal.quickConnect")}…`, onSelect: onQuickConnect },
    { kind: "separator", id: "s1" },
    { kind: "header", id: "h2", label: t("workspace.localShells") },
    ...localShells.map((l) => ({ id: `sh-${l.id}`, label: localShellName(l), shortcut: l.isDefault ? "Ctrl Shift T" : undefined, onSelect: () => onPickShell(l.id) })),
  ];

  return (
    <div className="tabbar">
      <div role="tablist" aria-label={t("workspace.newTab")} className="tabbar__tabs">
      {tabs.map((tab) => (
        <div
          key={tab.id}
          role="tab"
          tabIndex={0}
          aria-selected={tab.id === activeId}
          className="tab"
          data-tab-key={tab.id}
          onPointerDown={(e) => { if (!(e.target as Element).closest(".tab__close")) onTabPointerDown?.(e, tab.id); }}
          onClick={() => onActivate(tab.id)}
          onKeyDown={(e) => e.key === "Enter" && onActivate(tab.id)}
          onAuxClick={(e) => e.button === 1 && onClose(tab.id)}
        >
          {tab.kind === "local" ? <SquareTerminal size={13} aria-hidden="true" color="var(--accent)" /> : <StatusDot tone={dotTone[tab.status]} label={t(`workspace.status.${tab.status}`)} />}
          <span className="tab__title">{tab.title}</span>
          <span className="tab__sub pl-ellipsis">{tab.subtitle}</span>
          {tab.ai && <span className="ai-badge" title={t("mcp.aiTerminal")}>AI</span>}
          {tab.badge && <span className="pl-badge pl-badge--accent" style={{ height: 18 }}>{tab.badge}</span>}
          <span
            role="button"
            tabIndex={-1}
            aria-label={t("workspace.closeTab")}
            className="tab__close"
            onClick={(e) => { e.stopPropagation(); onClose(tab.id); }}
          >
            <X size={12} />
          </span>
        </div>
      ))}
      </div>
      <span className="pl-split" style={{ height: 30, flex: "none", background: "var(--accent-soft)", borderColor: "transparent" }}>
        <button type="button" className="pl-split__main" aria-label={t("workspace.newTab")} title={t("workspace.newTab")} style={{ padding: "0 8px" }} onClick={onNewDefault}>
          <Plus size={14} />
        </button>
        <button
          ref={moreRef}
          type="button"
          className="pl-split__more"
          aria-label={t("workspace.chooseTerminal")}
          aria-haspopup="menu"
          aria-expanded={!!menu}
          onClick={() => { const r = moreRef.current!.getBoundingClientRect(); setMenu({ x: r.left, y: r.bottom + 4 }); }}
        >
          <ChevronDown size={11} />
        </button>
      </span>
      <div style={{ flex: 1 }} />
      {broadcast && (
        <span className="broadcast-chip" role="status" title={`${t("terminal.broadcast.bar", { count: broadcast.count })}\n${broadcast.hint}`}>
          <Radio size={13} aria-hidden="true" />
          <span>{t("terminal.broadcast.chip", { count: broadcast.count })}</span>
          <button type="button" className="broadcast-chip__stop" onClick={broadcast.onStop}>{t("terminal.broadcast.stop")}</button>
        </span>
      )}
      <IconButton label={t("workspace.splitRight")} title={t("workspace.splitRight")} aria-haspopup="menu" onClick={(e) => onSplit(e.currentTarget)}><Columns2 size={15} /></IconButton>
      {onTogglePanel && (
        <IconButton label={t("panel.toggle")} title={t("panel.toggle")} aria-pressed={!!panelOpen} className={panelOpen ? "pl-icon-btn--on" : undefined} onClick={onTogglePanel}><PanelRight size={15} /></IconButton>
      )}
      {menu && <Menu x={menu.x} y={menu.y} entries={entries} label={t("workspace.chooseTerminal")} onClose={() => setMenu(null)} />}
    </div>
  );
}
