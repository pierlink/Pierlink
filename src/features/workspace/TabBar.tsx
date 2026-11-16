import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, Columns2, PanelLeft, PanelRight, Plus, Radio, SquareTerminal, X } from "lucide-react";
import { IconButton, Menu, StatusDot, useContextMenu, type MenuEntry } from "../../components";
import { localShellName } from "../terminal/backend";
import type { ServerView, TabView } from "./types";
import { maskHost, useHideAddresses } from "./hideAddresses";
import { useSettings } from "../settings/store";
import { useShortcutBindings, withShortcut } from "../shortcuts/runtime";
import { formatCombo } from "../shortcuts/keys";

const dotTone = { online: "ok", connecting: "warn", offline: "neutral" } as const;

/** 终端标签栏：标签、新建（默认终端 + 下拉选择服务器或本机终端）、分屏。 */
export function TabBar({ tabs, activeId, servers, localShells, onActivate, onClose, onCloseHover, onRename, onNewDefault, onPickServer, onPickShell, onQuickConnect, onSplit, onTabPointerDown, panelOpen, onTogglePanel, sidebarOpen, onToggleSidebar, broadcast }: {
  tabs: TabView[];
  activeId: string;
  servers: ServerView[];
  localShells: { id: string; name: string; isDefault: boolean }[];
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  /** 鼠标移到关闭按钮上、在标签上按下中键：快要关了（终端区先去查有没有程序在跑） */
  onCloseHover?: (id: string) => void;
  /**
   * 给标签起名字（右键“重命名”、双击标签、F2）。name 是填的内容（空的表示不起名字，用自动的）；
   * null 表示取消了。不传时标签不能改名
   */
  onRename?: (id: string, name: string | null) => void;
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
  /** 左边的服务器列表是否开着；不传 onToggleSidebar 时不显示收起按钮 */
  sidebarOpen?: boolean;
  onToggleSidebar?: () => void;
  /** 在标签上按下鼠标（拖动标签：并入别的标签的窗格、调整顺序） */
  onTabPointerDown?: (e: ReactPointerEvent, id: string) => void;
  /** 当前标签在广播输入：在标签栏右边显示一个小标记（不另占一行，开关广播时终端区不跳动） */
  broadcast?: { count: number; hint: string; onStop: () => void };
}) {
  const { t } = useTranslation();
  const moreRef = useRef<HTMLButtonElement>(null);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const hide = useHideAddresses();
  const keys = useShortcutBindings();
  // “新建终端标签”快捷键打开的本机终端：设置里选的，没选或已不存在时用检测出的默认项（和终端区的规则一样）
  const { settings } = useSettings();
  const defaultShell = localShells.find((x) => x.id === settings.terminal.defaultShell) ?? localShells.find((x) => x.isDefault) ?? localShells[0];
  const stripRef = useRef<HTMLDivElement>(null);
  // 正在改名的标签；width 是开始改名时标题的宽度，输入框照这个宽度放（标签不会一下子变宽变窄）
  const [renaming, setRenaming] = useState<{ id: string; width: number } | null>(null);
  const tabMenu = useContextMenu(t("workspace.tabMenu"));
  const startRename = (id: string, tabEl: HTMLElement) => {
    if (!onRename) return;
    const title = tabEl.querySelector<HTMLElement>(".tab__title");
    // 宽度不取整：取整会让标签宽零点几个像素，旁边的标签跟着挪一下
    setRenaming({ id, width: Math.max(64, title?.getBoundingClientRect().width ?? 0) });
  };
  // 鼠标在标签上时，滚轮左右滚动标签（不用去点细细的滚动条）。触控板左右滑动照常交给浏览器
  useEffect(() => {
    const el = stripRef.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      if (e.ctrlKey || Math.abs(e.deltaX) >= Math.abs(e.deltaY) || el.scrollWidth <= el.clientWidth) return;
      e.preventDefault();
      el.scrollLeft += e.deltaMode === 1 ? e.deltaY * 40 : e.deltaMode === 2 ? e.deltaY * el.clientWidth : e.deltaY;
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  const entries: MenuEntry[] = [
    { kind: "header", id: "h1", label: t("workspace.serversHeader") },
    ...servers.map((s) => ({ id: `srv-${s.id}`, label: hide ? maskHost(`${s.name}  ·  ${s.address}`, s.host) : `${s.name}  ·  ${s.address}`, onSelect: () => onPickServer(s.id) })),
    { id: "quick", label: `${t("terminal.quickConnect")}…`, shortcut: formatCombo(keys.quickConnect, " ") || undefined, onSelect: onQuickConnect },
    { kind: "separator", id: "s1" },
    { kind: "header", id: "h2", label: t("workspace.localShells") },
    ...localShells.map((l) => ({ id: `sh-${l.id}`, label: localShellName(l), shortcut: l.id === defaultShell?.id ? formatCombo(keys.newTab, " ") || undefined : undefined, onSelect: () => onPickShell(l.id) })),
  ];

  return (
    <div className="tabbar">
      {onToggleSidebar && (
        <IconButton label={t("workspace.serverList")} title={withShortcut(t, t("workspace.serverList"), formatCombo(keys.toggleSidebar))} aria-pressed={!!sidebarOpen} className={sidebarOpen ? "pl-icon-btn--on" : undefined} onClick={onToggleSidebar}><PanelLeft size={15} /></IconButton>
      )}
      <div ref={stripRef} role="tablist" aria-label={t("workspace.newTab")} className="tabbar__tabs">
      {tabs.map((tab) => (
        <div
          key={tab.id}
          role="tab"
          tabIndex={0}
          aria-selected={tab.id === activeId}
          className="tab"
          data-tab-key={tab.id}
          onPointerDown={(e) => {
            if (e.button === 1) onCloseHover?.(tab.id);
            if (!(e.target as Element).closest(".tab__close")) onTabPointerDown?.(e, tab.id);
          }}
          onClick={() => onActivate(tab.id)}
          onDoubleClick={(e) => { if (!(e.target as Element).closest(".tab__close")) startRename(tab.id, e.currentTarget); }}
          onKeyDown={(e) => {
            if (e.key === "Enter") onActivate(tab.id);
            else if (e.key === "F2") { e.preventDefault(); startRename(tab.id, e.currentTarget); }
          }}
          onAuxClick={(e) => e.button === 1 && onClose(tab.id)}
          onContextMenu={(e) => {
            const el = e.currentTarget;
            tabMenu.open(e, [
              ...(onRename ? [{ id: "rename", label: t("workspace.renameTab"), shortcut: "F2", onSelect: () => startRename(tab.id, el) }] : []),
              { id: "close", label: t("workspace.closeTab"), onSelect: () => onClose(tab.id) },
            ]);
          }}
        >
          {tab.kind === "local" ? <SquareTerminal size={13} aria-hidden="true" color="var(--accent)" /> : <StatusDot tone={dotTone[tab.status]} label={t(`workspace.status.${tab.status}`)} />}
          {tab.attention && <span className="tab__attention" role="img" aria-label={t("terminal.attention")} title={t("terminal.attention")} />}
          {renaming?.id === tab.id
            ? (
              <TabNameInput
                // 没起过名字时框里是自动的名字（全选着，直接打字就换掉）；清空后确定就是不起名字
                initial={tab.title}
                placeholder={tab.autoTitle ?? tab.title}
                label={t("workspace.tabName")}
                width={renaming.width}
                onDone={(value) => {
                  setRenaming(null);
                  // 没改、或者填的就是自动的名字：不算起了名字
                  onRename?.(tab.id, value === null || value === tab.title ? null : value.trim() === (tab.autoTitle ?? "") ? "" : value);
                }}
              />
            )
            : <span className="tab__title">{tab.title}</span>}
          <span className="tab__sub pl-ellipsis">{tab.subtitle}</span>
          {tab.ai && <span className="ai-badge" title={t("mcp.aiTerminal")}>AI</span>}
          {tab.badge && <span className="pl-badge pl-badge--accent" style={{ height: 18 }}>{tab.badge}</span>}
          <span
            role="button"
            tabIndex={-1}
            aria-label={t("workspace.closeTab")}
            className="tab__close"
            onPointerEnter={() => onCloseHover?.(tab.id)}
            onClick={(e) => { e.stopPropagation(); onClose(tab.id); }}
          >
            <X size={12} />
          </span>
        </div>
      ))}
      </div>
      <span className="pl-split" style={{ height: 30, flex: "none", background: "var(--accent-soft)", borderColor: "transparent" }}>
        <button type="button" className="pl-split__main" aria-label={t("workspace.newTab")} title={withShortcut(t, t("workspace.newTab"), formatCombo(keys.newTab))} style={{ padding: "0 8px" }} onClick={onNewDefault}>
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
      <IconButton label={t("workspace.splitRight")} title={withShortcut(t, t("workspace.splitRight"), formatCombo(keys.splitRight))} aria-haspopup="menu" onClick={(e) => onSplit(e.currentTarget)}><Columns2 size={15} /></IconButton>
      {onTogglePanel && (
        <IconButton label={t("panel.toggle")} title={withShortcut(t, t("panel.toggle"), formatCombo(keys.togglePanel))} aria-pressed={!!panelOpen} className={panelOpen ? "pl-icon-btn--on" : undefined} onClick={onTogglePanel}><PanelRight size={15} /></IconButton>
      )}
      {menu && <Menu x={menu.x} y={menu.y} entries={entries} label={t("workspace.chooseTerminal")} onClose={() => setMenu(null)} />}
      {tabMenu.node}
    </div>
  );
}

/**
 * 标签改名时的输入框：Enter 确定，Esc 取消，点到别处也算确定。
 * @param initial 框里一开始的内容（全选）
 * @param placeholder 清空后显示的字：不起名字时标签会显示的名字
 * @param label 给读屏用的名称
 * @param width 宽度（像素）：照改名前标题的宽度
 * @param onDone 结束：填的内容，取消时为 null
 */
function TabNameInput({ initial, placeholder, label, width, onDone }: { initial: string; placeholder: string; label: string; width: number; onDone: (value: string | null) => void }) {
  const done = useRef(false);
  const finish = (value: string | null) => {
    if (done.current) return;
    done.current = true;
    onDone(value);
  };
  return (
    <input
      // 一出现就拿到焦点并全选（双击标签时要赶在“切到这个标签后把焦点还给终端”之前）
      autoFocus
      onFocus={(e) => e.currentTarget.select()}
      className="tab__rename"
      style={{ width }}
      defaultValue={initial}
      placeholder={placeholder}
      aria-label={label}
      maxLength={200}
      spellCheck={false}
      // 输入框里的点击、按键只归输入框：不切标签、不拖标签、不触发终端快捷键
      onClick={(e) => e.stopPropagation()}
      onDoubleClick={(e) => e.stopPropagation()}
      onPointerDown={(e) => e.stopPropagation()}
      onAuxClick={(e) => e.stopPropagation()}
      onContextMenu={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter") { e.preventDefault(); finish(e.currentTarget.value); }
        if (e.key === "Escape") { e.preventDefault(); finish(null); }
      }}
      onBlur={(e) => finish(e.currentTarget.value)}
    />
  );
}
