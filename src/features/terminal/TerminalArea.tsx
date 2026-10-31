import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Bot, Columns2, FolderOpen, Gauge, Monitor, Plug, Radio, Rows2, Unplug, X } from "lucide-react";
import { Button, Dialog, Menu, TextField, type MenuEntry } from "../../components";
import { formatError } from "../../errors";
import { TabBar } from "../workspace/TabBar";
import type { ServerView, TabView } from "../workspace/types";
import { getTerminalBackend, listLocalShells, localShellName, type LocalShellView } from "./backend";
import { HostKeyDialog } from "./HostKeyDialog";
import { reorder, tabPart, zoneAt, zoneBox, type Box, type DragWhat, type DropAt } from "./paneDrag";
import { TerminalView, type TermHandle } from "./TerminalView";
import { DesktopPane } from "../desktop/DesktopPane";
import { focusDesktopWindow, openDesktopWindow, useDesktopPopouts } from "../desktop/popouts";
import { desktopWindowUrl } from "../desktop/DesktopWindow";
import type { Connecting, HostKeyStep, OpenSpec, RemoteSpec } from "./types";
import { parseQuickTarget } from "./util";
import { droppedText } from "./localPaths";
import { openInBrowser, writeClipboardText } from "../shell/clipboard";
import { inTauri } from "../shell/TitleBar";
import { useSettings } from "../settings/store";
import { computeRects, insertBeside, pairNodes, pairSpan, pane as leaf, paneIds, removePane, resizeAt, seriesCount, splitPane, swapPanes, type Dir, type Layout, type Rect, type Side } from "./panes";
import { FilePanel } from "../files/FilePanel";
import { loadWorkspace, maxPaneNumber, saveWorkspace, type SavedWorkspace } from "./persist";
import { TransferCenter } from "../files/TransferCenter";
import { isDangerous, LineBuffer, looksLikePasswordPrompt } from "./broadcast";
import { getMcpApi } from "../mcp/api";
import { pickPowerShell } from "../mcp/model";
import "../mcp/mcp.css";
import { ServerOverview } from "../monitor/ServerOverview";
import { useServerMonitor } from "../monitor/useServerMonitor";

import { overviewAuto as overviewDefault } from "../monitor/overviewPref";

/** 服务器终端的概况：采样一直开着（隐藏时放慢），显示时浮在终端右上角 */
function PaneOverview({ serverId, visible }: { serverId: string; visible: boolean }) {
  const { sample, state, login } = useServerMonitor(serverId, visible);
  return visible ? <ServerOverview serverId={serverId} sample={sample} state={state} onLogin={login} /> : null;
}

/** 一个窗格：一个终端会话 */
interface Pane {
  id: string;
  spec: OpenSpec;
  title: string;
  subtitle: string;
  status: "running" | "exited";
  /** 文件面板：是否打开、宽度（像素） */
  files: { open: boolean; width: number };
  /** 本机终端由提示符钩子报告的当前目录 */
  cwd: string | null;
  /** 终端里每按一次回车加一（服务器终端据此重新查询当前目录） */
  commands: number;
  /** 后端终端会话 ID（重新连接后会变；未连接为 null） */
  session: string | null;
  /** 应用重启后恢复的服务器窗格，还没连接（显示“未连接”，按 Enter 或点击连接） */
  restored: boolean;
  /** AI 打开的终端：request 为还没回报的打开请求编号；takenOver 为用户已接管；waiting 为 AI 在等用户输入密码 */
  ai?: { request: number | null; takenOver: boolean; waiting: boolean };
  /** 用户交给 AI 的终端：交给了哪个入口（entry 是 "windows" 或服务器 ID，label 是显示名） */
  handover?: { entry: string; label: string };
  /** 服务器终端右上角的概况是否显示（隐藏时采样不停） */
  overview: boolean;
  /** 服务器桌面（VNC）窗格，不是终端 */
  desktop?: boolean;
  /** 正在连接：窗格里的卡片从核对指纹一直显示到终端可用（连上或结束后清掉） */
  connecting?: Connecting;
  /** 手机正按手机宽度查看这个终端（手机的列数） */
  phoneCols?: number;
}


/** 窗格正在连接（核对指纹、登录到会话建立前）；核对没通过的不算 */
function isConnecting(p: Pane | undefined): boolean {
  return !!p?.connecting && p.connecting.phase !== "failed";
}

/** 一个标签：若干窗格的分屏布局 */
interface Tab {
  key: string;
  layout: Layout;
  /** 当前选中的窗格（标签栏显示它的标题） */
  active: string;
}

/** 新终端放在哪里：新标签（null），或者在某个窗格旁边拆分出来 */
type Target = { tab: string; pane: string; dir: Dir } | null;

type QuickSpec = Extract<OpenSpec, { kind: "quick" }>;
/** 一次远程连接：目标、标签上显示的名字；quick 为快速连接对话框里输入的原文（已保存服务器为 null） */
interface Conn {
  spec: RemoteSpec; title: string; subtitle: string; quick: string | null; target: Target; files?: boolean;
  /** 连接成功后激活这个已恢复的窗格（而不是新开一个） */
  revive?: string;
  /** AI 请求打开的终端：打开请求编号（打开结果要回报给后端） */
  ai?: number;
  /** 打开服务器桌面而不是终端 */
  desktop?: boolean;
  /** 开始连接的时间（performance.now()；快速连接在对话框里核对指纹，打开窗格后卡片接着计时） */
  startedAt?: number;
}
type Pending =
  | { kind: "paste"; text: string; resolve: (ok: boolean) => void }
  /** error：打开或复制失败的原因 */
  | { kind: "link"; url: string; error?: string }
  /** busy：正在检查服务器指纹 */
  | { kind: "quick"; value: string; error: string | null; busy?: boolean; target: Target }
  /** paneId：在窗格里连接时是哪个窗格（快速连接、桌面没有） */
  | { kind: "hostkey"; conn: Conn; step: Extract<HostKeyStep, { kind: "confirm" }>; busy: boolean; paneId?: string }
  /** 正在检查已保存服务器的指纹 */
  | { kind: "checking"; conn: Conn }
  /** 连接已保存服务器失败，可重试 */
  | { kind: "connectError"; conn: Conn; text: string }
  /** 关闭正在运行的标签（pane 为 null）或窗格前确认 */
  /** busy：只关一个终端时，里面是否有程序在运行（null 为查不到，或一次关多个） */
  | { kind: "close"; tab: string; pane: string | null; busy: boolean | null }
  /** 广播输入时回车前确认危险命令；rest 为确认后要发出的剩余输入（从这次回车开始） */
  | { kind: "danger"; tab: string; line: string; rest: string; source: string };

/** 文件面板最窄、终端最窄（像素）：窗格放不下两者时，文件面板改为浮在终端上面 */
const FILES_MIN = 220;
const TERM_MIN = 260;
const FILES_DEFAULT = 300;
/** 拖动分隔条时每个窗格的最小宽度、高度（像素） */
const PANE_MIN: Record<Dir, number> = { row: 200, col: 120 };

/** 窗格当前尺寸能不能再拆成两格（拆开后每格不小于最小尺寸）；找不到窗格元素时允许 */
function canSplit(paneId: string, dir: Dir): boolean {
  const el = document.querySelector(`[data-pane="${CSS.escape(paneId)}"]`);
  if (!el) return true;
  const r = el.getBoundingClientRect();
  if (!r.width || !r.height) return true;
  return (dir === "row" ? r.width : r.height) >= PANE_MIN[dir] * 2;
}

/** 读取上次的标签布局，转成初始状态。开发预览的 demo 网址不恢复（自动打开演示终端） */
function restoreWorkspace(): { tabs: Tab[]; panes: Record<string, Pane>; active: string | null; seq: number } {
  const empty = { tabs: [], panes: {}, active: null, seq: 0 };
  if (import.meta.env.DEV && window.location.hash.includes("demo")) return empty;
  const saved = loadWorkspace();
  if (!saved) return empty;
  let seq = maxPaneNumber(saved);
  const tabs: Tab[] = [];
  const panes: Record<string, Pane> = {};
  for (const t of saved.tabs) {
    for (const [id, p] of Object.entries(t.panes)) {
      panes[id] = {
        id, spec: p.spec, title: p.title, subtitle: p.subtitle, files: p.files, desktop: p.desktop,
        // 本机终端直接重新打开；服务器终端要用户确认后再连（可能要确认指纹、输入口令）
        status: p.spec.kind === "local" ? "running" : "exited",
        restored: p.spec.kind !== "local",
        cwd: null, commands: 0, session: null, overview: overviewDefault(),
      };
    }
    tabs.push({ key: `tab-${++seq}`, layout: t.layout, active: t.active });
  }
  return { tabs, panes, active: tabs[saved.active]?.key ?? tabs[0]?.key ?? null, seq };
}

/** 当前布局 → 保存的格式 */
function snapshot(tabs: Tab[], panes: Record<string, Pane>, active: string | null): SavedWorkspace {
  return {
    active: Math.max(0, tabs.findIndex((x) => x.key === active)),
    tabs: tabs.map((x) => ({
      layout: x.layout,
      active: x.active,
      panes: Object.fromEntries(paneIds(x.layout).filter((id) => panes[id]).map((id) => {
        const p = panes[id];
        return [id, { spec: p.spec, title: p.title, subtitle: p.subtitle, files: p.files, ...(p.desktop ? { desktop: true } : {}) }];
      })),
    })),
  };
}

/** 恢复出来、还没连接的服务器窗格：说明和“连接”按钮，按 Enter 也能连接 */
function RestoredPane({ title, subtitle, deleted, focus, closeLabel, onConnect, onClose }: {
  title: string;
  subtitle: string;
  /** 这台服务器已经被删除 */
  deleted: boolean;
  focus: boolean;
  closeLabel: string;
  onConnect: () => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const root = useRef<HTMLDivElement>(null);
  const connect = useRef(onConnect);
  connect.current = onConnect;
  // 当前窗格是它时按 Enter 连接：焦点在窗格里，或者焦点没在任何控件上（刚启动、点了空白处）
  useEffect(() => {
    if (!focus || deleted) return;
    const el = root.current;
    if (el && !el.contains(document.activeElement)) el.querySelector<HTMLButtonElement>(".term-ended__connect")?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Enter" || e.defaultPrevented || e.isComposing) return;
      const active = document.activeElement;
      const idle = !active || active === document.body || active === document.documentElement;
      if (!idle && !(el && el.contains(active))) return;
      e.preventDefault();
      connect.current();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [focus, deleted]);
  return (
    <div ref={root} className="term-pane" style={{ display: "flex" }}>
      <div className="term-ended term-ended--restored">
        <div className="term-ended__box">
          <div className="term-ended__title">{deleted ? t("terminal.restoredDeleted") : t("terminal.restoredTitle")}</div>
          <div className="term-ended__detail">
            <strong>{title}</strong> · <span className="mono">{subtitle}</span>
            <br />
            {deleted ? t("terminal.restoredDeletedDetail") : t("terminal.restoredDetail")}
          </div>
          <div className="term-ended__actions">
            <Button onClick={onClose}>{closeLabel}</Button>
            {!deleted && <Button variant="primary" className="term-ended__connect" onClick={onConnect}>{t("terminal.connect")}</Button>}
          </div>
          {!deleted && <div className="term-ended__hint">{t("terminal.restoredHint")}</div>}
        </div>
      </div>
    </div>
  );
}

/** 外部（服务器列表）请求打开某台服务器：新标签或在当前窗格右边分屏；每次请求的 n 不同 */
export interface OpenRequest { id: string; mode: "tab" | "split" | "desktop"; n: number }
/** 把命令片段发到终端：当前窗格，广播中则发给所有参与广播的窗格；execute 为真时再按一次回车。n 每次请求加一 */
export interface SnippetRequest { text: string; execute: boolean; n: number }

/** 终端区域：标签栏 + 每个标签一组分屏窗格（隐藏的标签和窗格都保持会话） */
export function TerminalArea({ servers, serversLoaded = true, openRequest, onConnections, onActiveServer, panelOpen, onTogglePanel, snippetRequest, onSnippetTargets }: {
  servers: ServerView[];
  /** 服务器列表已经读取过（之前不能据此判断恢复的窗格对应的服务器是否已删除） */
  serversLoaded?: boolean;
  openRequest?: OpenRequest | null;
  /** 每台服务器的连接状态（有已连上的终端 = 在线；正在检查指纹、连接中 = 连接中），变化时通知 */
  onConnections?: (status: Record<string, "online" | "connecting">) => void;
  /** 当前标签里选中的窗格所在的服务器（本机终端或没有标签时为 null），变化时通知 */
  onActiveServer?: (serverId: string | null) => void;
  /** 右侧工具面板的开关（按钮在标签栏右边） */
  panelOpen?: boolean;
  onTogglePanel?: () => void;
  /** 发送命令片段 */
  snippetRequest?: SnippetRequest | null;
  /** 命令片段现在会发到几个窗格（0 = 没有可用的终端），变化时通知 */
  onSnippetTargets?: (count: number) => void;
}) {
  const { t } = useTranslation();
  const backend = getTerminalBackend();
  const { settings } = useSettings();
  const [shells, setShells] = useState<LocalShellView[]>([]);
  // 上次关闭应用时的标签布局：本机终端直接重新打开，服务器终端显示“未连接”等用户确认
  const [restored] = useState(restoreWorkspace);
  const [tabs, setTabs] = useState<Tab[]>(restored.tabs);
  const [panes, setPanes] = useState<Record<string, Pane>>(restored.panes);
  const [activeTab, setActiveTab] = useState<string | null>(restored.active);
  const [pending, setPending] = useState<Pending | null>(null);
  const [splitMenu, setSplitMenu] = useState<{ x: number; y: number; tab: string; pane: string; dir: Dir } | null>(null);
  // “交给 AI”菜单：选哪个入口的 AI 接手
  const [aiMenu, setAiMenu] = useState<{ x: number; y: number; entries: MenuEntry[] } | null>(null);
  const seq = useRef(restored.seq);
  // 异步回调里要读最新的标签和窗格
  const tabsRef = useRef<Tab[]>([]);
  tabsRef.current = tabs;
  const panesRef = useRef<Record<string, Pane>>({});
  panesRef.current = panes;
  const activeTabRef = useRef<string | null>(null);
  activeTabRef.current = activeTab;
  // 每次发起连接加一；用户中途关掉对话框后，迟到的结果据此丢弃
  const attempt = useRef(0);

  // 广播输入：每个标签参与广播的窗格（没有这个键 = 没开广播）
  const [broadcast, setBroadcast] = useState<Record<string, string[]>>({});
  const broadcastRef = useRef(broadcast);
  broadcastRef.current = broadcast;
  // 广播时的提示（某个窗格退出了广播等），几秒后消失
  const [broadcastNotice, setBroadcastNotice] = useState<string | null>(null);
  const noticeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const showBroadcastNotice = (text: string) => {
    setBroadcastNotice(text);
    if (noticeTimer.current) clearTimeout(noticeTimer.current);
    noticeTimer.current = setTimeout(() => setBroadcastNotice(null), 8000);
  };
  // 每个标签一份按键行缓冲（判断危险命令用）
  const lineBuffers = useRef(new Map<string, LineBuffer>());
  // 每个终端窗格的操作句柄（命令片段用）
  const handles = useRef(new Map<string, TermHandle>());
  /** 从资源管理器拖着文件经过的本机终端窗格（高亮提示松手会填入路径） */
  const [fileDrop, setFileDrop] = useState<string | null>(null);
  useEffect(() => {
    if (!inTauri) return;
    /** 拖放位置（相对窗口的物理像素）下面是不是一个运行中的本机终端 */
    const localPaneAt = (pos: { x: number; y: number }): string | null => {
      const ratio = window.devicePixelRatio || 1;
      const id = document.elementFromPoint(pos.x / ratio, pos.y / ratio)?.closest<HTMLElement>("[data-pane]")?.dataset.pane;
      const p = id ? panesRef.current[id] : undefined;
      return id && p && p.spec.kind === "local" && !p.desktop && p.status === "running" ? id : null;
    };
    let off: (() => void) | undefined;
    let alive = true;
    void import("@tauri-apps/api/webview")
      .then(({ getCurrentWebview }) => getCurrentWebview().onDragDropEvent((ev) => {
        const d = ev.payload;
        if (d.type === "leave") { setFileDrop(null); return; }
        const id = localPaneAt(d.position);
        if (d.type !== "drop") { setFileDrop(id); return; }
        setFileDrop(null);
        const p = id ? panesRef.current[id] : undefined;
        if (!id || p?.spec.kind !== "local" || !d.paths.length) return;
        // 只填路径不回车；遵守括号粘贴模式，和粘贴一样
        const h = handles.current.get(id);
        h?.paste(droppedText(p.spec.shellId, d.paths));
        h?.focus();
        void import("@tauri-apps/api/window").then(({ getCurrentWindow }) => getCurrentWindow().setFocus()).catch(() => undefined);
      }))
      .then((u) => { if (alive) off = u; else u(); }, (e: unknown) => console.error(formatError(e)));
    return () => { alive = false; off?.(); };
  }, []);
  /** 用户自己打开（或点“连接”恢复）的终端：挂载后把焦点给它，不用先点一下；AI 打开的终端不抢焦点 */
  const focusOnMount = useRef(new Set<string>());

  // 本机终端检测完之前点了“默认本机终端”：记下来，检测完自动打开（不能当作什么都没发生）
  const [shellsLoaded, setShellsLoaded] = useState(false);
  const pendingDefault = useRef<{ target: Target } | null>(null);
  useEffect(() => {
    void listLocalShells().then(setShells, () => setShells([])).finally(() => setShellsLoaded(true));
  }, []);

  // 布局变化后稍等一下保存（拖分隔条时不会每一帧都写）
  useEffect(() => {
    const timer = setTimeout(() => saveWorkspace(snapshot(tabs, panes, activeTab)), 400);
    return () => clearTimeout(timer);
  }, [tabs, panes, activeTab]);

  /** 打开一个终端：新标签，或在指定窗格旁拆分。返回新窗格 ID */
  const add = (spec: OpenSpec, title: string, subtitle: string, target: Target, files = false, ai?: number, desktop = false, connecting?: Connecting): string => {
    const id = `pane-${++seq.current}`;
    const aiState = ai === undefined ? undefined : { request: ai, takenOver: false, waiting: false };
    if (ai === undefined) focusOnMount.current.add(id);
    const pane: Pane = { id, spec, title, subtitle, status: "running", files: { open: files, width: FILES_DEFAULT }, cwd: null, commands: 0, session: null, restored: false, ai: aiState, overview: overviewDefault(), ...(desktop ? { desktop: true } : {}), ...(connecting ? { connecting } : {}) };
    panesRef.current = { ...panesRef.current, [id]: pane };
    setPanes((ps) => ({ ...ps, [id]: pane }));
    const tab = target && tabsRef.current.find((x) => x.key === target.tab);
    if (target && tab && paneIds(tab.layout).includes(target.pane)) {
      const next = (x: Tab) => (x.key === target.tab ? { ...x, layout: splitPane(x.layout, target.pane, target.dir, id), active: id } : x);
      tabsRef.current = tabsRef.current.map(next);
      setTabs((ts) => ts.map(next));
      setActiveTab(target.tab);
    } else {
      const key = `tab-${++seq.current}`;
      tabsRef.current = [...tabsRef.current, { key, layout: leaf(id), active: id }];
      setTabs((ts) => [...ts, { key, layout: leaf(id), active: id }]);
      setActiveTab(key);
    }
    return id;
  };
  const openShellIn = (shellId: string, target: Target): string | null => {
    const s = shells.find((x) => x.id === shellId);
    return s ? add({ kind: "local", shellId: s.id }, localShellName(s), t("workspace.local"), target) : null;
  };
  const openDefault = (target: Target): string | null => {
    if (!shellsLoaded) { pendingDefault.current = { target }; return null; }
    // 设置里选了默认终端且仍存在就用它，否则用检测出的默认项
    const d = shells.find((x) => x.id === settings.terminal.defaultShell) ?? shells.find((x) => x.isDefault) ?? shells[0];
    return d ? openShellIn(d.id, target) : null;
  };

  useEffect(() => {
    if (!shellsLoaded || !pendingDefault.current) return;
    const { target } = pendingDefault.current;
    pendingDefault.current = null;
    openDefault(target);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shellsLoaded]);

  // 开发时网址带 demo（如 #workspace-demo）就自动打开预览终端，方便截图检查
  useEffect(() => {
    if (!import.meta.env.DEV || !shells.length || tabs.length) return;
    const hash = window.location.hash;
    // #…-hostkey-demo：演示首次连接的指纹确认框
    if (hash.includes("hostkey-demo")) openQuick("dev@203.0.113.18", null);
    else if (hash.includes("split-demo")) {
      // #workspace-split-demo：左右分屏，右边再上下分
      const first = openDefault(null);
      const tab = tabsRef.current.at(-1);
      if (first && tab) {
        const right = openShellIn(shells[1]?.id ?? shells[0].id, { tab: tab.key, pane: first, dir: "row" });
        if (right) openShellIn(shells[2]?.id ?? shells[0].id, { tab: tab.key, pane: right, dir: "col" });
      }
    } else if (hash.includes("demo")) openDefault(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shells]);

  /** 处理指纹确认的一步：已信任就打开终端，否则弹出确认框 */
  const onHostKeyStep = (my: number, conn: Conn, step: HostKeyStep) => {
    if (my !== attempt.current) {
      if (step.kind === "confirm") void backend.cancelHostKey(step.probeId);
      return;
    }
    if (step.kind === "trusted") {
      setPending(null);
      if (conn.revive) updatePane(conn.revive, () => ({ restored: false }));
      else if (conn.desktop && conn.spec.kind === "server" && settings.desktop.openInWindow) openWindowDesktop(conn, conn.spec.serverId);
      else {
        const now = performance.now();
        const connecting: Connecting | undefined = conn.desktop ? undefined : { start: conn.startedAt ?? now, stepStart: now, phase: "go" };
        add(conn.spec, conn.title, conn.subtitle, conn.target, conn.files, conn.ai, conn.desktop, connecting);
      }
      return;
    }
    setPending({ kind: "hostkey", conn, step, busy: false });
  };
  /** 设置里选了“在独立窗口打开”：指纹确认后直接开独立窗口，主窗口不开窗格（窗口里自己检测、连接） */
  const openWindowDesktop = (conn: Conn, serverId: string) => {
    openDesktopWindow(serverId, conn.title, desktopWindowUrl(serverId, conn.title)).catch((err: unknown) => {
      setPending({ kind: "connectError", conn, text: formatError(err) });
    });
  };
  const onHostKeyError = (my: number, conn: Conn, err: unknown) => {
    if (my !== attempt.current) return;
    if (conn.ai !== undefined) reportAiOpen(conn.ai, null, err);
    // 用户自己拒绝的不算错误，直接关闭
    if ((err as { code?: string } | null)?.code === "ssh.host_key_rejected") { setPending(null); return; }
    if (conn.quick !== null) setPending({ kind: "quick", value: conn.quick, error: formatError(err), target: conn.target });
    else setPending({ kind: "connectError", conn, text: formatError(err) });
  };

  // ---------- 在窗格里连接服务器 ----------
  /** 每个窗格这次连接用的参数（重试、取消时用）和核对指纹的轮次（迟到的结果丢掉） */
  const paneConns = useRef<Record<string, Conn>>({});
  const probeSeq = useRef<Record<string, number>>({});
  /** 同时有几个窗格要确认指纹时排队，一次只弹一个 */
  const hostKeyQueue = useRef<Extract<Pending, { kind: "hostkey" }>[]>([]);
  const pendingRef = useRef<Pending | null>(null);
  pendingRef.current = pending;

  /** 打开服务器终端：窗格马上出现，卡片从核对指纹一直显示到终端可用 */
  const connectPane = (conn: Conn) => {
    const now = performance.now();
    // 同一台服务器半秒内又来一次（双击按成了两次）：当成同一次
    if (!conn.revive && conn.ai === undefined && Object.values(panesRef.current).some((p) =>
      p.connecting?.phase === "probe" && now - p.connecting.start < 500 && JSON.stringify(p.spec) === JSON.stringify(conn.spec))) return;
    const connecting: Connecting = { start: now, stepStart: now, phase: "probe" };
    let id: string;
    if (conn.revive) {
      id = conn.revive;
      updatePane(id, () => ({ restored: false, connecting }));
    } else {
      id = add(conn.spec, conn.title, conn.subtitle, conn.target, conn.files, conn.ai, false, connecting);
    }
    paneConns.current[id] = conn;
    probePane(id);
  };
  /** 核对一个窗格要连的服务器的指纹 */
  const probePane = (id: string) => {
    const conn = paneConns.current[id];
    if (!conn) return;
    const my = (probeSeq.current[id] ?? 0) + 1;
    probeSeq.current[id] = my;
    backend.checkHostKey(conn.spec).then(
      (step) => onPaneHostKey(id, my, step),
      (err: unknown) => onPaneProbeError(id, my, err),
    );
  };
  /** 这一轮核对还算数（没取消、没重试、窗格还在） */
  const probeCurrent = (id: string, my: number) => probeSeq.current[id] === my && !!panesRef.current[id];
  const onPaneHostKey = (id: string, my: number, step: HostKeyStep) => {
    if (!probeCurrent(id, my)) {
      if (step.kind === "confirm") void backend.cancelHostKey(step.probeId);
      return;
    }
    const now = performance.now();
    if (step.kind === "trusted") {
      updatePane(id, (p) => ({ connecting: p.connecting && { ...p.connecting, phase: "go", stepStart: now } }));
      return;
    }
    updatePane(id, (p) => ({ connecting: p.connecting && { ...p.connecting, phase: "confirm", stepStart: p.connecting.phase === "confirm" ? p.connecting.stepStart : now } }));
    const item = { kind: "hostkey" as const, conn: paneConns.current[id], step, busy: false, paneId: id };
    // 已经有对话框（另一个指纹确认、快速连接框、出错提示）：排队，那个关掉后再问
    if (pendingRef.current) hostKeyQueue.current.push(item);
    else showPending(item);
  };
  const onPaneProbeError = (id: string, my: number, err: unknown) => {
    if (!probeCurrent(id, my)) return;
    const code = (err as { code?: string } | null)?.code ?? "";
    // 用户自己拒绝了指纹：不算错误，取消这次连接
    if (code === "ssh.host_key_rejected") { abandonPane(id, err); return; }
    // AI 打开的终端：把原因告诉 AI，不留下失败的窗格
    if (panesRef.current[id]?.ai?.request != null) { abandonPane(id, err); return; }
    updatePane(id, (p) => ({ connecting: p.connecting && { ...p.connecting, phase: "failed", error: formatError(err), failedAt: code.includes("host_key") ? "verify" : "connect" } }));
  };
  /** 显示对话框，并马上记下（同一时刻又来一个指纹确认时能看到前一个，排队而不是盖掉它） */
  const showPending = (next: Pending | null) => {
    pendingRef.current = next;
    setPending(next);
  };
  /** 弹出排队的下一个指纹确认（窗格已经关掉的跳过；没有就关掉对话框） */
  const nextHostKey = () => {
    let next = hostKeyQueue.current.shift();
    while (next?.paneId && !panesRef.current[next.paneId]) {
      void backend.cancelHostKey(next.step.probeId);
      next = hostKeyQueue.current.shift();
    }
    showPending(next ?? null);
  };
  // 等指纹确认的窗格被关掉了（关了标签）：取消这次确认，接着问下一个；
  // 别的对话框关掉了、有排着队的指纹确认：弹出来
  useEffect(() => {
    if (pending?.kind === "hostkey" && pending.paneId && !panes[pending.paneId]) {
      void backend.cancelHostKey(pending.step.probeId);
      nextHostKey();
    } else if (!pending && hostKeyQueue.current.length) nextHostKey();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pending, panes]);
  /**
   * 放弃一个窗格的连接：恢复出来的窗格回到“未连接”，新开的关掉。
   * @param aiErr AI 还在等这个终端时回报的原因（null 不回报）
   */
  const abandonPane = (id: string, aiErr: unknown) => {
    probeSeq.current[id] = (probeSeq.current[id] ?? 0) + 1;
    const conn = paneConns.current[id];
    delete paneConns.current[id];
    hostKeyQueue.current = hostKeyQueue.current.filter((h) => {
      if (h.paneId !== id) return true;
      void backend.cancelHostKey(h.step.probeId);
      return false;
    });
    const request = panesRef.current[id]?.ai?.request;
    if (request != null && aiErr) reportAiOpen(request, null, aiErr);
    if (conn?.revive) { updatePane(id, () => ({ restored: true, connecting: undefined })); return; }
    const tab = tabsRef.current.find((x) => paneIds(x.layout).includes(id));
    if (tab) dropPanes(tab.key, [id]);
  };
  /** 连接卡片上点“取消” */
  const cancelPane = (id: string) => {
    const cur = pendingRef.current;
    if (cur?.kind === "hostkey" && cur.paneId === id) {
      void backend.cancelHostKey(cur.step.probeId);
      nextHostKey();
    }
    abandonPane(id, { code: "ssh.auth_cancelled", params: {} });
  };
  /** 核对指纹失败后点“重试”或按 Enter */
  const retryPane = (id: string) => {
    const now = performance.now();
    updatePane(id, () => ({ connecting: { start: now, stepStart: now, phase: "probe" } }));
    probePane(id);
  };
  /** 回答窗格里连接的指纹确认 */
  const answerPaneHostKey = (h: Extract<Pending, { kind: "hostkey" }>, accept: boolean) => {
    const id = h.paneId as string;
    const my = probeSeq.current[id] ?? 0;
    if (!accept) {
      void backend.cancelHostKey(h.step.probeId);
      nextHostKey();
      abandonPane(id, { code: "ssh.host_key_rejected", params: {} });
      return;
    }
    if (h.busy) return;
    showPending({ ...h, busy: true });
    // 等回答期间这个窗格可能被关掉或取消，对话框已经换成了下一个：那时不能再弹一次，会把下一个顶掉
    const stillShown = () => {
      const cur = pendingRef.current;
      return cur?.kind === "hostkey" && cur.paneId === id && cur.step.probeId === h.step.probeId;
    };
    backend.answerHostKey(h.step.probeId, true).then(
      (next) => {
        // 同一台服务器还有下一级要确认（跳板）：直接接着问，不排到别的窗格后面
        if (next.kind === "confirm" && probeCurrent(id, my)) showPending({ ...h, step: next, busy: false });
        else { if (stillShown()) nextHostKey(); onPaneHostKey(id, my, next); }
      },
      (err: unknown) => { if (stillShown()) nextHostKey(); onPaneProbeError(id, my, err); },
    );
  };

  /** 远程连接（快速连接、服务器桌面）：先在应用内确认指纹，通过后再打开 */
  const connect = (conn: Conn) => {
    const my = ++attempt.current;
    // 正在问某个窗格的指纹：放回队首，这边的对话框关掉后接着问
    const cur = pendingRef.current;
    if (cur?.kind === "hostkey" && cur.paneId && !cur.busy) hostKeyQueue.current.unshift(cur);
    setPending(conn.quick !== null ? { kind: "quick", value: conn.quick, error: null, busy: true, target: conn.target } : { kind: "checking", conn });
    backend.checkHostKey(conn.spec).then(
      (step) => onHostKeyStep(my, conn, step),
      (err: unknown) => onHostKeyError(my, conn, err),
    );
  };
  const openQuick = (value: string, target: Target) => {
    const spec: QuickSpec | null = parseQuickTarget(value);
    if (!spec) { setPending({ kind: "quick", value, error: t("terminal.invalidTarget"), target }); return; }
    connect({ spec, title: spec.host, subtitle: spec.user ? `${spec.user}@${spec.host}` : spec.host, quick: value, target, startedAt: performance.now() });
  };
  const popouts = useDesktopPopouts();
  const openServer = (id: string, target: Target, desktop = false) => {
    const s = servers.find((x) => x.id === id);
    if (!s) return;
    // 每台服务器只开一个桌面：已经弹到独立窗口里了，就把那个窗口提到前面；主窗口里已经开着，就切到那个窗格
    if (desktop && popouts.has(id)) { void focusDesktopWindow(id); return; }
    if (desktop) {
      const open = Object.values(panesRef.current).find((p) => p.desktop && p.spec.kind === "server" && p.spec.serverId === id);
      const tab = open && tabsRef.current.find((x) => paneIds(x.layout).includes(open.id));
      if (open && tab) { setActiveTab(tab.key); focusPane(tab.key, open.id); return; }
    }
    if (desktop) connect({ spec: { kind: "server", serverId: s.id }, title: t("desktop.paneTitle", { name: s.name }), subtitle: s.address, quick: null, target, desktop: true });
    else connectPane({ spec: { kind: "server", serverId: s.id }, title: s.name, subtitle: s.address, quick: null, target });
  };
  /** 连接一个恢复出来的服务器窗格：照常先确认指纹，通过后在原窗格里打开终端 */
  const revive = (paneId: string) => {
    const p = panesRef.current[paneId];
    focusOnMount.current.add(paneId);
    if (!p || p.spec.kind === "local") { updatePane(paneId, () => ({ restored: false })); return; }
    if (p.spec.kind === "server" && !servers.some((x) => x.id === (p.spec as { serverId: string }).serverId)) return;
    connectPane({ spec: p.spec, title: p.title, subtitle: p.subtitle, quick: null, target: null, revive: paneId });
  };
  /** 当前标签当前窗格旁边（没有标签、或当前窗格太小放不下两格时为新标签） */
  const currentTarget = (dir: Dir): Target => {
    const tab = tabsRef.current.find((x) => x.key === activeTabRef.current);
    return tab && canSplit(tab.active, dir) ? { tab: tab.key, pane: tab.active, dir } : null;
  };
  /**
   * 把 AI 打开终端的结果回报给后端（成功给会话 ID，失败给原因）。
   * 后端已经不等了（超过 120 秒）时，paneId 对应的窗格去掉 AI 标记，变成普通终端
   */
  const reportAiOpen = (request: number, session: string | null, err: unknown, paneId?: string) => {
    void getMcpApi().then((api) => api.terminalOpened(request, session, err)).then(
      (accepted) => { if (!accepted && paneId) updatePane(paneId, () => ({ ai: undefined })); },
      (e: unknown) => console.error(e),
    );
  };
  // AI 请求打开终端：服务器走正常连接流程（指纹、密码照常由用户处理）；本机打开 PowerShell
  useEffect(() => {
    let alive = true;
    let off: (() => void) | undefined;
    void getMcpApi().then((api) => api.onOpenTerminal((r) => {
      if (r.kind === "server") {
        const s = servers.find((x) => x.id === r.serverId);
        if (!s) { reportAiOpen(r.id, null, { code: "server.not_found", params: {} }); return; }
        connectPane({ spec: { kind: "server", serverId: s.id }, title: r.title ?? s.name, subtitle: s.address, quick: null, target: null, ai: r.id });
        return;
      }
      const shell = pickPowerShell(shells);
      if (!shell) { reportAiOpen(r.id, null, { code: "terminal.shell_not_found", params: { shell_id: "powershell" } }); return; }
      add({ kind: "local", shellId: shell.id }, r.title ?? localShellName(shell), t("workspace.local"), null, false, r.id);
    })).then((u) => { if (alive) off = u; else u(); });
    return () => { alive = false; off?.(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [servers, shells]);
  // 手机按手机宽度查看某个终端：在那个窗格上提示（电脑上打字就恢复）
  useEffect(() => {
    if (backend.preview) return;
    let alive = true;
    let off: (() => void) | undefined;
    void import("@tauri-apps/api/event").then(({ listen }) => listen<{ terminalId: string; cols: number | null }>("pierlink://phone-size", (e) => {
      const pane = Object.values(panesRef.current).find((x) => x.session === e.payload.terminalId);
      if (pane) updatePane(pane.id, () => ({ phoneCols: e.payload.cols ?? undefined }));
    })).then((u) => { if (alive) off = u; else u(); });
    return () => { alive = false; off?.(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  // AI 终端在等密码：在那个窗格上提示用户自己输入
  useEffect(() => {
    let alive = true;
    let off: (() => void) | undefined;
    void getMcpApi().then((api) => api.onNotice((n) => {
      if (n.kind === "terminalHandedOver" || n.kind === "terminalTakenBack") {
        const pane = Object.values(panesRef.current).find((x) => x.session === n.terminalId);
        if (pane) updatePane(pane.id, () => ({ handover: n.kind === "terminalHandedOver" ? { entry: n.entry, label: entryName(n.entry, n.entryLabel) } : undefined }));
        return;
      }
      if (n.kind !== "passwordPrompt") return;
      const pane = Object.values(panesRef.current).find((x) => x.session === n.terminalId && x.ai);
      if (pane) updatePane(pane.id, (x) => ({ ai: x.ai && { ...x.ai, waiting: true } }));
    })).then((u) => { if (alive) off = u; else u(); });
    return () => { alive = false; off?.(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  /** 入口的显示名：本机入口翻译，服务器入口用服务器名称 */
  const entryName = (entry: string, label: string) => (entry === "windows" ? t("mcp.entryWindows") : label);
  /** 把用户终端交给某个入口的 AI；成功后窗格显示 AI 标记（后端事件也会再设一次） */
  const handOver = (paneId: string, session: string, entry: string, label: string) => {
    void getMcpApi().then((api) => api.handOver(session, entry)).then(
      () => updatePane(paneId, () => ({ handover: { entry, label } })),
      (e: unknown) => showBroadcastNotice(t("terminal.handover.failed", { reason: formatError(e) })),
    );
  };
  /** 打开“交给 AI”菜单：列出能接手这个终端的入口；没有时只显示原因 */
  const openHandoverMenu = (el: Element, paneId: string) => {
    const session = panesRef.current[paneId]?.session;
    if (!session) return;
    const r = el.getBoundingClientRect();
    void getMcpApi().then((api) => api.handoverEntries(session)).then((list) => {
      // 只有一个入口能接手时直接交，不用再选
      if (list.length === 1) { handOver(paneId, session, list[0].entry, entryName(list[0].entry, list[0].label)); return; }
      const entries: MenuEntry[] = list.length === 0
        // 没有能接手的入口：用会自动换行的说明，长句子也能看全
        ? [{ kind: "hint", id: "none", label: t("terminal.handover.none") }]
        : [
            { kind: "header", id: "h", label: t("terminal.handover.menuHeader") },
            ...list.map((x) => {
              const label = entryName(x.entry, x.label);
              return { id: `e-${x.entry}`, label, onSelect: () => handOver(paneId, session, x.entry, label) };
            }),
          ];
      setAiMenu({ x: Math.max(8, r.right - 300), y: r.bottom + 4, entries });
    }, (e: unknown) => showBroadcastNotice(t("terminal.handover.failed", { reason: formatError(e) })));
  };
  /** 收回交给 AI 的终端 */
  const takeBack = (paneId: string) => {
    const session = panesRef.current[paneId]?.session;
    if (!session) return;
    void getMcpApi().then((api) => api.takeBack(session)).then(
      () => updatePane(paneId, () => ({ handover: undefined })),
      (e: unknown) => showBroadcastNotice(t("terminal.handover.failed", { reason: formatError(e) })),
    );
  };
  /** 把当前目录报给后端（AI 列终端时显示）；还没有会话时不报 */
  const reportCwd = (session: string | null, cwd: string | null) => {
    if (!session) return;
    void getMcpApi().then((api) => api.reportCwd(session, cwd)).catch((e: unknown) => console.error(formatError(e)));
  };
  /** 用户接管 AI 终端（拒绝 AI 的输入）或交还给 AI */
  const toggleTakeOver = (id: string) => {
    const p = panesRef.current[id];
    if (!p?.ai || !p.session) return;
    const takenOver = !p.ai.takenOver;
    void getMcpApi().then((api) => api.setUserControl(p.session as string, takenOver)).then(
      () => updatePane(id, (x) => ({ ai: x.ai && { ...x.ai, takenOver } })),
      (e: unknown) => console.error(formatError(e)),
    );
  };
  /** 终端会话变化：AI 打开的终端第一次拿到会话 ID 时回报给后端 */
  const onPaneSession = (id: string, session: string | null) => {
    const p = panesRef.current[id];
    if (session) { delete paneConns.current[id]; updatePane(id, (x) => (x.connecting ? { connecting: undefined } : {})); }
    if (session && p?.ai && p.ai.request !== null) {
      reportAiOpen(p.ai.request, session, null, id);
      updatePane(id, (x) => ({ session, ai: x.ai && { ...x.ai, request: null } }));
    } else updatePane(id, () => ({ session, handover: undefined }));
    if (session && p?.cwd) reportCwd(session, p.cwd);
  };
  /** 终端打开失败：AI 还在等这个终端时回报原因 */
  const onPaneError = (id: string, err: unknown) => {
    console.error(formatError(err));
    const p = panesRef.current[id];
    if (p?.ai && p.ai.request !== null) {
      reportAiOpen(p.ai.request, null, err);
      updatePane(id, (x) => ({ ai: x.ai && { ...x.ai, request: null } }));
    }
  };
  // 外部（服务器列表）请求打开某台服务器：每次请求的 n 不同
  useEffect(() => {
    if (openRequest) openServer(openRequest.id, openRequest.mode === "split" ? currentTarget("row") : null, openRequest.mode === "desktop");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [openRequest]);

  const answerHostKey = (accept: boolean) => {
    if (pending?.kind !== "hostkey") return;
    if (pending.paneId) { answerPaneHostKey(pending, accept); return; }
    const { conn, step } = pending;
    const my = attempt.current;
    if (!accept) {
      // 回答进行中也可以取消：迟到的结果会因 attempt 变化被丢弃，若是下一级提示会被自动取消
      attempt.current++;
      setPending(null);
      void backend.cancelHostKey(step.probeId);
      if (conn.ai !== undefined) reportAiOpen(conn.ai, null, { code: "ssh.host_key_rejected", params: {} });
      return;
    }
    if (pending.busy) return;
    setPending({ ...pending, busy: true });
    backend.answerHostKey(step.probeId, true).then(
      (next) => onHostKeyStep(my, conn, next),
      (err: unknown) => onHostKeyError(my, conn, err),
    );
  };
  const closeQuick = () => { attempt.current++; setPending(null); };

  /** 去掉一些窗格；标签里的窗格全去掉时连标签一起关掉 */
  const dropPanes = (tabKey: string, ids: string[]) => {
    const before = tabsRef.current;
    const index = before.findIndex((x) => x.key === tabKey);
    if (index < 0) return;
    let layout: Layout | null = before[index].layout;
    for (const id of ids) layout = layout && removePane(layout, id);
    const next: Tab[] = [];
    for (const x of before) {
      if (x.key !== tabKey) next.push(x);
      else if (layout) {
        const left = paneIds(layout);
        next.push({ ...x, layout, active: left.includes(x.active) ? x.active : left[left.length - 1] });
      }
    }
    tabsRef.current = next;
    setTabs(next);
    // 关掉的是当前标签：切到原位置上的下一个（没有就前一个）
    if (!layout && activeTabRef.current === tabKey) setActiveTab(next[Math.min(index, next.length - 1)]?.key ?? null);
    setPanes((ps) => { const out = { ...ps }; for (const id of ids) delete out[id]; return out; });
  };
  /**
   * 关闭前确认（设置里可整体关掉）：
   * - 一次关多个还连着的终端（关整个分屏标签）：确认；
   * - 只关一个：终端里有程序在运行（如 codex、vim）才确认，shell 空闲时直接关；查不到是否在运行时按在运行处理；
   * - 已断开、已结束的直接关闭。
   */
  const closeCheck = useRef<string | null>(null);
  const requestClose = (tabKey: string, paneId: string | null) => {
    const tab = tabsRef.current.find((x) => x.key === tabKey);
    if (!tab) return;
    const ids = paneId ? [paneId] : paneIds(tab.layout);
    // 桌面窗格关了只是断开查看，服务器上的桌面照常运行，不用确认
    const live = ids.filter((id) => panesRef.current[id]?.status === "running" && !panesRef.current[id]?.desktop);
    if (!live.length || !settings.terminal.confirmClose) { dropPanes(tabKey, ids); return; }
    if (live.length > 1) { setPending({ kind: "close", tab: tabKey, pane: paneId, busy: null }); return; }
    const session = panesRef.current[live[0]]?.session;
    const key = `${tabKey}/${paneId ?? ""}`;
    if (closeCheck.current === key) return;
    closeCheck.current = key;
    // 查询期间（服务器上要另开一条 SSH 连接）鼠标显示为“正在处理”，避免以为没点上
    document.body.classList.add("pane-checking");
    // 服务器终端要另外建一条 SSH 连接查询，最多等 3 秒
    const timeout = new Promise<null>((r) => setTimeout(() => r(null), 3000));
    void Promise.race([session ? backend.busy(session).catch(() => null) : Promise.resolve(null), timeout]).then((busy) => {
      closeCheck.current = null;
      document.body.classList.remove("pane-checking");
      const p = panesRef.current[live[0]];
      if (!p || p.status !== "running") { dropPanes(tabKey, ids); return; }
      if (busy === false) { dropPanes(tabKey, ids); return; }
      setPending({ kind: "close", tab: tabKey, pane: paneId, busy });
    });
  };
  // ---------- 拖动窗格和标签 ----------
  // 拖窗格标题栏：放到另一个窗格的某一边（或中间交换位置）、放到别的标签上（并入，默认在右边）、放到标签栏空白处（单独成为新标签）。
  // 拖标签：放到窗格的某一边（整个标签并进去）、放到别的标签上（中间并入，两侧调整顺序）。
  // 拖着停在别的标签上一会儿会切过去，可以再放到那个标签的窗格上。
  const [drag, setDrag] = useState<{ what: DragWhat; label: string; x: number; y: number; drop: DropAt | null; box: Box | null; bad: boolean } | null>(null);
  /** 刚拖完：吞掉松手后跟着来的 click（否则会去激活已经被合并掉的标签） */
  const suppressClick = useRef(false);
  const commitTabs = (next: Tab[], active: string | null) => {
    tabsRef.current = next;
    setTabs(next);
    if (active) setActiveTab(active);
  };
  /** 放下后的标签列表和要切到的标签（不改状态；放下无效时返回 null） */
  const planDrop = (what: DragWhat, at: DropAt): { next: Tab[]; active: string | null; anchor?: string; moved?: string[] } | null => {
    const list = tabsRef.current;
    const src = list.find((x) => x.key === what.tab);
    if (!src) return null;
    // 拖动期间窗格可能已经被关掉（关闭前的查询是异步的）
    if (what.kind === "pane" && !paneIds(src.layout).includes(what.id)) return null;
    if (at.kind !== "newtab" && !list.some((x) => x.key === at.tab)) return null;
    if (at.kind === "tab" && at.part !== "merge") {
      return what.kind === "tab" ? { next: reorder(list, (x) => x.key, what.tab, at.tab, at.part), active: null } : null;
    }
    if (at.kind === "pane" && at.side === "swap") {
      if (what.kind !== "pane" || at.tab !== what.tab) return null;
      return { next: list.map((x) => (x.key === at.tab ? { ...x, layout: swapPanes(x.layout, what.id, at.pane) } : x)), active: null };
    }
    const node: Layout = what.kind === "pane" ? leaf(what.id) : src.layout;
    const moved = paneIds(node);
    const focusId = what.kind === "pane" ? what.id : src.active;
    // 先从原来的标签里拿出来（拖整个标签，或标签里只剩这一个窗格时，原标签消失）
    const rest = what.kind === "pane" ? removePane(src.layout, what.id) : null;
    const next: Tab[] = list.flatMap((x) => {
      if (x.key !== src.key) return [x];
      if (!rest) return [];
      const left = paneIds(rest);
      return [{ ...x, layout: rest, active: left.includes(x.active) ? x.active : left[left.length - 1] }];
    });
    if (at.kind === "newtab") {
      if (!rest) return null;
      const key = `tab-new`;
      next.splice(next.findIndex((x) => x.key === src.key) + 1, 0, { key, layout: node, active: focusId });
      return { next, active: key };
    }
    const target = next.find((x) => x.key === at.tab);
    if (!target) return null;
    const anchor = at.kind === "pane" ? at.pane : target.active;
    if (moved.includes(anchor)) return null;
    const side: Side = at.kind === "pane" ? (at.side as Side) : "right";
    return {
      next: next.map((x) => (x.key === at.tab ? { ...x, layout: insertBeside(x.layout, anchor, side, node), active: focusId } : x)),
      active: at.tab,
      anchor,
      moved,
    };
  };
  /** 执行放下 */
  const applyDrop = (what: DragWhat, at: DropAt) => {
    const plan = planDrop(what, at);
    if (!plan) return;
    // 新标签的 key 在真正放下时才分配
    const key = `tab-${++seq.current}`;
    const next = plan.next.map((x) => (x.key === "tab-new" ? { ...x, key } : x));
    commitTabs(next, plan.active === "tab-new" ? key : plan.active);
  };
  /** 放到窗格旁边后，移过去的窗格和目标窗格是否都不小于最小尺寸 */
  const fits = (what: DragWhat, at: DropAt): boolean => {
    const plan = planDrop(what, at);
    const grid = document.querySelector(".pane-grid")?.getBoundingClientRect();
    if (!plan || !plan.anchor || !grid) return !!plan;
    const tab = plan.next.find((x) => x.key === plan.active);
    if (!tab) return false;
    const rects = computeRects(tab.layout).panes;
    return [plan.anchor, ...(plan.moved ?? [])].every((id) => {
      const r = rects[id];
      return !!r && (r.w * grid.width) / 100 >= PANE_MIN.row - 1 && (r.h * grid.height) / 100 >= PANE_MIN.col - 1;
    });
  };
  /** 指针下面是什么、放下会怎样 */
  const hitTest = (what: DragWhat, x: number, y: number): { drop: DropAt | null; box: Box | null; bad: boolean } => {
    const none = { drop: null, box: null, bad: false };
    const el = document.elementFromPoint(x, y);
    const tabEl = el?.closest<HTMLElement>("[data-tab-key]");
    if (tabEl) {
      const key = tabEl.dataset.tabKey!;
      if (key === what.tab) return none;
      const r = tabEl.getBoundingClientRect();
      const part = tabPart((x - r.left) / r.width, what.kind === "tab");
      const box = part === "before" ? { left: r.left - 2, top: r.top, width: 3, height: r.height }
        : part === "after" ? { left: r.right - 1, top: r.top, width: 3, height: r.height }
        : { left: r.left, top: r.top, width: r.width, height: r.height };
      return { drop: { kind: "tab", tab: key, part }, box, bad: false };
    }
    if (el?.closest(".tabbar")) {
      const src = tabsRef.current.find((t) => t.key === what.tab);
      if (what.kind !== "pane" || !src || paneIds(src.layout).length < 2) return none;
      return { drop: { kind: "newtab" }, box: null, bad: false };
    }
    const slot = el?.closest<HTMLElement>("[data-pane]");
    if (!slot) return none;
    const id = slot.dataset.pane!;
    const tab = slot.dataset.paneTab!;
    if ((what.kind === "pane" && id === what.id) || (what.kind === "tab" && tab === what.tab)) return none;
    const rr = slot.getBoundingClientRect();
    const r = { left: rr.left, top: rr.top, width: rr.width, height: rr.height };
    const side = zoneAt((x - r.left) / r.width, (y - r.top) / r.height, what.kind === "pane" && tab === what.tab);
    const drop: DropAt = { kind: "pane", tab, pane: id, side };
    const bad = side !== "swap" && !fits(what, drop);
    return { drop: bad ? null : drop, box: zoneBox(r, side), bad };
  };
  /** 按下窗格标题栏或标签：移动超过几像素才算开始拖 */
  const beginDrag = (e: ReactPointerEvent, what: DragWhat, label: string) => {
    if (e.button !== 0) return;
    const sx = e.clientX;
    const sy = e.clientY;
    let started = false;
    let latest: ReturnType<typeof hitTest> = { drop: null, box: null, bad: false };
    let hoverTab: string | null = null;
    let hoverTimer: ReturnType<typeof setTimeout> | null = null;
    const move = (ev: PointerEvent) => {
      if (!started) {
        if (Math.hypot(ev.clientX - sx, ev.clientY - sy) < 6) return;
        started = true;
        document.body.classList.add("pane-dragging");
      }
      latest = hitTest(what, ev.clientX, ev.clientY);
      // 停在别的标签上 0.6 秒：切过去，可以接着放到它的窗格上
      const over = latest.drop?.kind === "tab" ? latest.drop.tab : null;
      if (over !== hoverTab) {
        hoverTab = over;
        if (hoverTimer) clearTimeout(hoverTimer);
        hoverTimer = over ? setTimeout(() => setActiveTab(over), 600) : null;
      }
      setDrag({ what, label, x: ev.clientX, y: ev.clientY, ...latest });
    };
    const finish = (apply: boolean) => {
      window.removeEventListener("pointermove", move, true);
      window.removeEventListener("pointerup", up, true);
      window.removeEventListener("pointercancel", cancel, true);
      window.removeEventListener("keydown", key, true);
      if (hoverTimer) clearTimeout(hoverTimer);
      document.body.classList.remove("pane-dragging");
      setDrag(null);
      if (!started) return;
      suppressClick.current = true;
      setTimeout(() => { suppressClick.current = false; }, 0);
      if (apply && latest.drop) applyDrop(what, latest.drop);
    };
    const up = () => finish(true);
    const cancel = () => finish(false);
    const key = (ev: KeyboardEvent) => { if (ev.key === "Escape") { ev.preventDefault(); finish(false); } };
    window.addEventListener("pointermove", move, true);
    window.addEventListener("pointerup", up, true);
    window.addEventListener("pointercancel", cancel, true);
    window.addEventListener("keydown", key, true);
  };
  const dropHint = (d: NonNullable<typeof drag>): string => {
    if (d.bad) return t("terminal.drop.tooSmall");
    const at = d.drop;
    if (!at) return "";
    if (at.kind === "newtab") return t("terminal.drop.newTab");
    if (at.kind === "tab") {
      const name = tabViewsRef.current.find((x) => x.id === at.tab)?.title ?? "";
      return at.part === "merge" ? t("terminal.drop.merge", { name }) : t("terminal.drop.reorder");
    }
    return t(`terminal.drop.${at.side}`);
  };

  const updatePane = (id: string, patch: (p: Pane) => Partial<Pane>) =>
    setPanes((ps) => (ps[id] ? { ...ps, [id]: { ...ps[id], ...patch(ps[id]) } } : ps));
  const setStatus = (id: string, status: Pane["status"]) => {
    setPanes((ps) => (ps[id] && ps[id].status !== status ? { ...ps, [id]: { ...ps[id], status, ...(status === "exited" ? { connecting: undefined, phoneCols: undefined } : {}) } } : ps));
    // 断开或退出的窗格自动退出广播；重新连上后不自动加回（避免把输入送进刚登录的 shell）
    if (status === "exited") for (const [key, ids] of Object.entries(broadcastRef.current)) if (ids.includes(id)) leaveBroadcast(key, id, null);
  };

  // ---------- 广播输入 ----------
  /** 标签里能参与广播的窗格：已连接、正在运行 */
  const broadcastable = (tabKey: string) => {
    const tab = tabsRef.current.find((x) => x.key === tabKey);
    return tab ? paneIds(tab.layout).filter((id) => panesRef.current[id]?.session && panesRef.current[id]?.status === "running" && !panesRef.current[id]?.restored) : [];
  };
  /** 开关当前标签的广播输入（Ctrl+Shift+B）：打开时所有已连接的窗格都参与 */
  const toggleBroadcast = (tabKey: string) => {
    lineBuffers.current.delete(tabKey);
    if (broadcastRef.current[tabKey]) {
      setBroadcast((b) => { const n = { ...b }; delete n[tabKey]; return n; });
      return;
    }
    const ids = broadcastable(tabKey);
    if (ids.length < 2) { showBroadcastNotice(t("terminal.broadcast.needTwo")); return; }
    setBroadcast((b) => ({ ...b, [tabKey]: ids }));
  };
  /** 单个窗格加入或退出广播 */
  const toggleMember = (tabKey: string, paneId: string) => {
    const ids = broadcastRef.current[tabKey];
    if (!ids) return;
    if (ids.includes(paneId)) leaveBroadcast(tabKey, paneId, null);
    else if (broadcastable(tabKey).includes(paneId)) setBroadcast((b) => ({ ...b, [tabKey]: [...(b[tabKey] ?? []), paneId] }));
  };
  /** 窗格退出广播；reason 不为空时提示用户 */
  const leaveBroadcast = (tabKey: string, paneId: string, reason: string | null) => {
    setBroadcast((b) => (b[tabKey]?.includes(paneId) ? { ...b, [tabKey]: b[tabKey].filter((x) => x !== paneId) } : b));
    if (reason) showBroadcastNotice(reason);
  };
  /** 把一段输入发给标签里参与广播的所有窗格 */
  const sendToBroadcast = (tabKey: string, data: string, source: string) => {
    for (const id of broadcastRef.current[tabKey] ?? []) {
      const p = panesRef.current[id];
      if (!p?.session || p.status !== "running") continue;
      backend.write(p.session, data).catch((e: unknown) => showBroadcastNotice(t("common.labeled", { label: p.title, text: formatError(e) })));
      // 来源窗格自己的回车计数在 TerminalView 里已经加过
      if (id !== source && /[\r\n]/.test(data)) updatePane(id, (x) => ({ commands: x.commands + 1 }));
    }
  };
  /**
   * 广播中的窗格收到键盘输入：发给所有参与的窗格。回车时如果这一行是危险命令，先停下来确认。
   * @returns 是否已接管（true 时窗格自己不再发送）
   */
  const onBroadcastInput = (tabKey: string, paneId: string, data: string): boolean => {
    const ids = broadcastRef.current[tabKey];
    if (!ids?.includes(paneId)) return false;
    let buf = lineBuffers.current.get(tabKey);
    if (!buf) { buf = new LineBuffer(); lineBuffers.current.set(tabKey, buf); }
    if (settings.terminal.broadcastConfirmDanger && /[\r\n]/.test(data)) {
      // 这段输入里每次回车完成的一行都要检查（一次粘贴可能有多行，危险命令可能夹在中间）
      const probe = new LineBuffer();
      probe.feed(buf.current);
      const danger = probe.feed(data).find(isDangerous);
      if (danger !== undefined) {
        if (data.includes("\x1b[200~")) {
          // 括号粘贴：整段都等确认后再发（从中间截断会让对方 shell 停在粘贴状态）
          setPending({ kind: "danger", tab: tabKey, line: danger, rest: data, source: paneId });
          return true;
        }
        // 普通按键：危险那一行回车之前的部分照常发出，从那次回车开始的剩余输入等确认后再发
        const cut = new LineBuffer();
        cut.feed(buf.current);
        let at = 0;
        for (const m of data.matchAll(/\r\n?|\n/g)) {
          const done = cut.feed(data.slice(at, m.index + m[0].length));
          at = m.index + m[0].length;
          if (done.some(isDangerous)) { at = m.index; break; }
        }
        const before = data.slice(0, at);
        buf.feed(before);
        if (before) sendToBroadcast(tabKey, before, paneId);
        setPending({ kind: "danger", tab: tabKey, line: danger, rest: data.slice(at), source: paneId });
        return true;
      }
    }
    buf.feed(data);
    sendToBroadcast(tabKey, data, paneId);
    return true;
  };
  /** 广播中的窗格有新输出：最后一行像密码提示时，这个窗格退出广播（密码不能广播） */
  const onBroadcastOutput = (tabKey: string, paneId: string, tail: string) => {
    if (!broadcastRef.current[tabKey]?.includes(paneId)) return;
    if (looksLikePasswordPrompt(tail, lineBuffers.current.get(tabKey)?.current ?? "")) leaveBroadcast(tabKey, paneId, t("terminal.broadcast.leftForPassword", { name: panesRef.current[paneId]?.title ?? "" }));
  };
  // 标签关闭、窗格关闭后清理；没有窗格参与时自动结束广播（只剩一个时保留，方便输完密码后再把别的窗格加回来）
  useEffect(() => {
    const next: Record<string, string[]> = {};
    let changed = false;
    for (const [key, ids] of Object.entries(broadcast)) {
      const tab = tabs.find((x) => x.key === key);
      const alive = tab ? ids.filter((id) => paneIds(tab.layout).includes(id) && panes[id]) : [];
      if (alive.length >= 1) next[key] = alive;
      if (alive.length !== ids.length || alive.length === 0) changed = true;
    }
    if (changed) {
      for (const key of Object.keys(broadcast)) if (!next[key]) lineBuffers.current.delete(key);
      setBroadcast(next);
    }
  }, [broadcast, tabs, panes]);
  const focusPane = (tabKey: string, paneId: string) =>
    setTabs((ts) => ts.map((x) => (x.key === tabKey && x.active !== paneId ? { ...x, active: paneId } : x)));

  // ---------- 命令片段 ----------
  // 片段发到哪：广播中 = 参与广播的窗格（只需交给其中一个，由广播分发）；否则当前窗格
  const snippetTargets = (() => {
    const tab = tabs.find((x) => x.key === activeTab);
    if (!tab) return 0;
    if (broadcast[tab.key]) return broadcast[tab.key].length;
    const p = panes[tab.active];
    return p?.session && p.status === "running" ? 1 : 0;
  })();
  useEffect(() => { onSnippetTargets?.(snippetTargets); }, [snippetTargets, onSnippetTargets]);
  const lastSnippet = useRef(0);
  useEffect(() => {
    if (!snippetRequest || snippetRequest.n === lastSnippet.current) return;
    lastSnippet.current = snippetRequest.n;
    const tab = tabsRef.current.find((x) => x.key === activeTabRef.current);
    if (!tab) return;
    const members = broadcastRef.current[tab.key];
    // 广播中：交给参与广播的一个窗格（优先当前窗格），它的输入会分发给全部
    const via = members ? (members.includes(tab.active) ? tab.active : members[0]) : tab.active;
    const h = handles.current.get(via);
    if (!h) return;
    h.paste(snippetRequest.text);
    if (snippetRequest.execute) h.input("\r");
    h.focus();
  }, [snippetRequest]);

  // 当前窗格所在的服务器（右侧面板跟随它）
  const activeServer = (() => {
    const tab = tabs.find((x) => x.key === activeTab);
    const spec = tab ? panes[tab.active]?.spec : undefined;
    return spec?.kind === "server" ? spec.serverId : null;
  })();
  useEffect(() => { onActiveServer?.(activeServer); }, [activeServer, onActiveServer]);

  // 汇总每台服务器的连接状态给服务器列表（“在线”筛选、状态圆点）
  const connectionsKey = useRef("");
  useEffect(() => {
    const status: Record<string, "online" | "connecting"> = {};
    for (const p of Object.values(panes)) {
      if (p.spec.kind !== "server") continue;
      if (p.connecting) {
        if (isConnecting(p) && !status[p.spec.serverId]) status[p.spec.serverId] = "connecting";
        continue;
      }
      if (p.status === "running") status[p.spec.serverId] = "online";
    }
    const connecting = pending?.kind === "checking" || pending?.kind === "hostkey" ? pending.conn.spec : null;
    if (connecting?.kind === "server" && !status[connecting.serverId]) status[connecting.serverId] = "connecting";
    const key = JSON.stringify(status);
    if (key !== connectionsKey.current) { connectionsKey.current = key; onConnections?.(status); }
  }, [panes, pending, onConnections]);

  const tabViewsRef = useRef<TabView[]>([]);
  const tabViews: TabView[] = tabs.flatMap((x) => {
    const ids = paneIds(x.layout);
    const p = panes[x.active] ?? panes[ids[0]];
    if (!p) return [];
    return [{
      id: x.key,
      kind: p.spec.kind === "local" ? "local" : "remote",
      title: p.title,
      subtitle: p.subtitle,
      // 有窗格正在连接（核对指纹到会话建立前）显示“连接中”；核对没通过的不算在线
      status: ids.some((id) => isConnecting(panes[id])) ? "connecting"
        : ids.some((id) => panes[id]?.status === "running" && panes[id]?.connecting?.phase !== "failed") ? "online" : "offline",
      badge: ids.length > 1 ? t("terminal.paneCount", { count: ids.length }) : undefined,
      ai: ids.some((id) => !!panes[id]?.ai || !!panes[id]?.handover),
    }];
  });
  tabViewsRef.current = tabViews;

  /** 在窗格旁边开一个同样的终端：同一台服务器，或同一种本机终端 */
  const splitSame = (tab: string, paneId: string, dir: Dir) => {
    const cur = panesRef.current[paneId];
    if (!cur) return;
    const target: Target = { tab, pane: paneId, dir };
    if (cur.spec.kind === "local") openShellIn(cur.spec.shellId, target);
    else if (cur.spec.kind === "server") openServer(cur.spec.serverId, target);
    else connect({ spec: cur.spec, title: cur.title, subtitle: cur.subtitle, quick: null, target });
  };

  /** 分屏菜单：新窗格里打开哪个终端。窗格太小、拆开后会小于最小尺寸时只显示原因 */
  const splitEntries = (m: NonNullable<typeof splitMenu>): MenuEntry[] => {
    if (!canSplit(m.pane, m.dir)) {
      return [{ id: "small", label: m.dir === "row" ? t("terminal.tooNarrow") : t("terminal.tooShort"), disabled: true }];
    }
    const target: Target = { tab: m.tab, pane: m.pane, dir: m.dir };
    const cur = panes[m.pane];
    const same: MenuEntry[] = cur
      ? [{
          id: "same",
          label: t("terminal.splitSame", { name: cur.title }),
          onSelect: () => splitSame(m.tab, m.pane, m.dir),
        }, { kind: "separator", id: "s0" }]
      : [];
    return [
      ...same,
      { kind: "header", id: "h1", label: t("workspace.localShells") },
      ...shells.map((l) => ({ id: `sh-${l.id}`, label: localShellName(l), onSelect: () => openShellIn(l.id, target) })),
      { kind: "separator", id: "s1" },
      { kind: "header", id: "h2", label: t("workspace.serversHeader") },
      ...servers.map((s) => ({ id: `srv-${s.id}`, label: `${s.name}  ·  ${s.address}`, onSelect: () => openServer(s.id, target) })),
      { id: "quick", label: `${t("terminal.quickConnect")}…`, onSelect: () => setPending({ kind: "quick", value: "", error: null, target }) },
    ];
  };
  const openSplitMenu = (el: Element, tab: string, paneId: string, dir: Dir) => {
    const r = el.getBoundingClientRect();
    setSplitMenu({ x: Math.max(8, r.right - 260), y: r.bottom + 4, tab, pane: paneId, dir });
  };

  const closing = pending?.kind === "close" ? (() => {
    const tab = tabs.find((x) => x.key === pending.tab);
    if (!tab) return null;
    const ids = pending.pane ? [pending.pane] : paneIds(tab.layout);
    return { ids, first: panes[ids[0]], anyLocal: ids.some((id) => panes[id]?.spec.kind === "local") };
  })() : null;

  return (
    <section className="ws__main" aria-label={t("workspace.newTab")}>
      <TabBar
        broadcast={activeTab && broadcast[activeTab] ? { count: broadcast[activeTab].length, hint: t("terminal.broadcast.hint"), onStop: () => toggleBroadcast(activeTab) } : undefined}
        tabs={tabViews}
        activeId={activeTab ?? ""}
        servers={servers}
        localShells={shells}
        onActivate={(key) => { if (!suppressClick.current) setActiveTab(key); }}
        onTabPointerDown={(e, key) => {
          const v = tabViews.find((x) => x.id === key);
          const tab = tabs.find((x) => x.key === key);
          const count = tab ? paneIds(tab.layout).length : 1;
          beginDrag(e, { kind: "tab", tab: key }, count > 1 ? t("terminal.drop.tabLabel", { name: v?.title ?? "", count }) : v?.title ?? "");
        }}
        onClose={(key) => requestClose(key, null)}
        onNewDefault={() => openDefault(null)}
        onPickShell={(id) => openShellIn(id, null)}
        onPickServer={(id) => openServer(id, null)}
        onQuickConnect={() => setPending({ kind: "quick", value: "", error: null, target: null })}
        panelOpen={panelOpen}
        onTogglePanel={onTogglePanel}
        onSplit={(el) => {
          const tab = tabs.find((x) => x.key === activeTab);
          if (tab) openSplitMenu(el, tab.key, tab.active, "row");
          else openDefault(null);
        }}
      />
      {tabs.length === 0 && (
        <div className="term-placeholder term-placeholder--actions">
          <Button variant="primary" onClick={() => openDefault(null)}>{t("terminal.localDefault")}</Button>
          <Button onClick={() => setPending({ kind: "quick", value: "", error: null, target: null })}>{t("terminal.quickConnect")}</Button>
          {backend.preview && <span style={{ fontSize: "var(--fs-xs)" }}>{t("terminal.previewBanner")}</span>}
        </div>
      )}
      {/* 广播时偶尔的提醒：浮在终端上方，不挡点击、不挤动终端，几秒后消失 */}
      {broadcastNotice && <div className="broadcast-notice" role="alert">{broadcastNotice}</div>}
      <PaneStage
        tabs={tabs}
        activeTab={activeTab}
        onResize={(key, layout) => setTabs((ts) => ts.map((x) => (x.key === key ? { ...x, layout } : x)))}
        render={(id, tab) => {
              const multi = paneIds(tab.layout).length > 1;
              const visible = tab.key === activeTab;
              const p = panes[id];
              if (!p) return null;
              const active = tab.active === id;
              const members = broadcast[tab.key];
              const inBroadcast = !!members?.includes(id);
              return (
                <div className={`pane${multi ? " pane--multi" : ""}${multi && active ? " pane--active" : ""}${inBroadcast ? " pane--broadcast" : ""}`} onPointerDownCapture={() => focusPane(tab.key, id)}>
                  <PaneBody
                    bar={p.desktop ? null : (
                      <>
                      <div className="pane__bar" role="toolbar" aria-label={p.title}>
                        <span className="pane__title pl-ellipsis" title={p.cwd ?? p.subtitle} onPointerDown={(e) => beginDrag(e, { kind: "pane", id, tab: tab.key }, p.title)}>
                        {multi && <strong>{p.title}</strong>}
                        {p.cwd && <span className="mono">{p.cwd}</span>}
                      </span>
                        {p.handover && (
                          <span className="pane__ai" title={t("terminal.handover.chipTitle")}>
                            <span className="ai-badge">AI</span>
                            <span className="pl-ellipsis">{p.handover.entry === "windows" ? t("terminal.handover.chipWindows") : t("terminal.handover.chip", { name: p.handover.label })}</span>
                            <button type="button" className="pane__ai-back" onClick={() => takeBack(id)}>{t("terminal.handover.takeBack")}</button>
                          </span>
                        )}
                        {p.spec.kind !== "local" && !p.handover && !(p.ai && !p.ai.takenOver) && (() => {
                          // 服务器终端的“连接 / 断开”：连着时断开（内容留着），没连着时连接（和按 Enter 一样）。
                          // AI 正在用的终端不给：断开后 AI 那边收不了尾，要断开先收回
                          const live = !p.restored && p.status === "running";
                          const label = live ? t("terminal.disconnect") : t("terminal.connect");
                          return (
                            <button type="button" className="pane__btn" title={label} aria-label={label}
                              onClick={() => {
                                const h = handles.current.get(id);
                                if (p.restored) revive(id);
                                else if (live) h?.disconnect();
                                else h?.reconnect();
                                h?.focus();
                              }}>
                              {live ? <Unplug size={14} /> : <Plug size={14} />}
                            </button>
                          );
                        })()}
                        {!p.handover && !p.ai && p.session && p.spec.kind !== "quick" && (
                          <button type="button" className="pane__btn" title={t("terminal.handover.button")} aria-label={t("terminal.handover.button")} aria-haspopup="menu"
                            onClick={(e) => openHandoverMenu(e.currentTarget, id)}>
                            <Bot size={14} />
                          </button>
                        )}
                        {p.spec.kind === "server" && !p.restored && (
                          <button type="button" className={`pane__btn${p.overview ? " pane__btn--on" : ""}`} aria-pressed={p.overview}
                            title={p.overview ? t("monitor.hide") : t("monitor.show")} aria-label={t("monitor.show")}
                            onClick={() => updatePane(id, (x) => ({ overview: !x.overview }))}>
                            <Gauge size={14} />
                          </button>
                        )}
                        {p.spec.kind === "server" && !p.restored && (
                          <button type="button" className="pane__btn" title={t("desktop.open")} aria-label={t("desktop.open")}
                            onClick={() => openServer((p.spec as { serverId: string }).serverId, canSplit(id, "row") ? { tab: tab.key, pane: id, dir: "row" } : null, true)}>
                            <Monitor size={14} />
                          </button>
                        )}
                        <button type="button" className={`pane__btn${p.files.open ? " pane__btn--on" : ""}`} aria-pressed={p.files.open} title={t("terminal.files")} aria-label={t("terminal.files")}
                          onClick={() => updatePane(id, (x) => ({ files: { ...x.files, open: !x.files.open } }))}>
                          <FolderOpen size={14} />
                        </button>
                        <button type="button" className="pane__btn pane__btn--split" title={t("workspace.splitRight")} aria-label={t("workspace.splitRight")} aria-haspopup="menu"
                          onClick={(e) => openSplitMenu(e.currentTarget, tab.key, id, "row")}>
                          <Columns2 size={14} />
                        </button>
                        <button type="button" className="pane__btn pane__btn--split" title={t("workspace.splitDown")} aria-label={t("workspace.splitDown")} aria-haspopup="menu"
                          onClick={(e) => openSplitMenu(e.currentTarget, tab.key, id, "col")}>
                          <Rows2 size={14} />
                        </button>
                        {multi && !p.restored && (
                          <button type="button" className={`pane__btn${inBroadcast ? " pane__btn--broadcast" : ""}`}
                            aria-pressed={members ? inBroadcast : false}
                            title={members ? (inBroadcast ? t("terminal.broadcast.leavePane") : t("terminal.broadcast.joinPane")) : t("terminal.broadcast.start")}
                            aria-label={members ? (inBroadcast ? t("terminal.broadcast.leavePane") : t("terminal.broadcast.joinPane")) : t("terminal.broadcast.start")}
                            onClick={() => (members ? toggleMember(tab.key, id) : toggleBroadcast(tab.key))}>
                            <Radio size={14} />
                          </button>
                        )}
                        {multi && (
                          <button type="button" className="pane__btn pane__btn--close" title={t("terminal.closePane")} aria-label={t("terminal.closePane")} onClick={() => requestClose(tab.key, id)}>
                            <X size={14} />
                          </button>
                        )}
                      </div>
                      {p.ai && (
                        <div className={`ai-bar${p.ai.waiting ? " ai-bar--warn" : ""}`} role="status">
                          <span className="ai-badge">AI</span>
                          <span className="ai-bar__text pl-ellipsis">{p.ai.waiting ? t("mcp.aiWaitingPassword") : p.ai.takenOver ? t("mcp.aiTakenOver") : t("mcp.aiActive")}</span>
                          {p.ai.waiting && <Button size="sm" variant="ghost" onClick={() => updatePane(id, (x) => ({ ai: x.ai && { ...x.ai, waiting: false } }))}>{t("mcp.gotIt")}</Button>}
                          {p.session && <Button size="sm" onClick={() => toggleTakeOver(id)}>{p.ai.takenOver ? t("mcp.handBack") : t("mcp.takeOver")}</Button>}
                        </div>
                      )}
                      </>
                    )}
                    files={p.files}
                    onFilesWidth={(width) => updatePane(id, (x) => ({ files: { ...x.files, width } }))}
                    panel={p.files.open && !p.restored ? (
                      <FilePanel
                        paneId={id}
                        spec={p.spec}
                        title={p.title}
                        cwd={p.cwd}
                        commands={p.commands}
                        sessionId={p.session}
                        onClose={() => updatePane(id, (x) => ({ files: { ...x.files, open: false } }))}
                      />
                    ) : null}
                  >
                    {p.restored ? (
                      <RestoredPane
                        title={p.title}
                        subtitle={p.subtitle}
                        deleted={serversLoaded && p.spec.kind === "server" && !servers.some((x) => p.spec.kind === "server" && x.id === p.spec.serverId)}
                        focus={visible && tab.active === id}
                        closeLabel={multi ? t("terminal.closePane") : t("terminal.closeTab")}
                        onConnect={() => revive(id)}
                        onClose={() => dropPanes(tab.key, [id])}
                      />
                    ) : p.desktop && p.spec.kind === "server" ? (
                      <DesktopPane
                        serverId={p.spec.serverId}
                        visible={visible}
                        windowTitle={p.title}
                        onPoppedOut={() => dropPanes(tab.key, [id])}
                        title={(
                          <span className="pane__title pl-ellipsis" title={p.subtitle} onPointerDown={(e) => beginDrag(e, { kind: "pane", id, tab: tab.key }, p.title)}>
                            <strong>{p.title}</strong>
                          </span>
                        )}
                        controls={(
                          <>
                            <button type="button" className="pane__btn pane__btn--split" title={t("workspace.splitRight")} aria-label={t("workspace.splitRight")} aria-haspopup="menu"
                              onClick={(e) => openSplitMenu(e.currentTarget, tab.key, id, "row")}>
                              <Columns2 size={14} />
                            </button>
                            <button type="button" className="pane__btn pane__btn--split" title={t("workspace.splitDown")} aria-label={t("workspace.splitDown")} aria-haspopup="menu"
                              onClick={(e) => openSplitMenu(e.currentTarget, tab.key, id, "col")}>
                              <Rows2 size={14} />
                            </button>
                            {multi && (
                              <button type="button" className="pane__btn pane__btn--close" title={t("terminal.closePane")} aria-label={t("terminal.closePane")} onClick={() => requestClose(tab.key, id)}>
                                <X size={14} />
                              </button>
                            )}
                          </>
                        )}
                      />
                    ) : (
                    <>
                    {/* 服务器终端连上后开始采样概况；隐藏时也接着采（放慢），切回来马上有数 */}
                    {p.spec.kind === "server" && p.session && (
                      <PaneOverview serverId={p.spec.serverId} visible={p.overview && visible} />
                    )}
                    <TerminalView
                      backend={backend}
                      spec={p.spec}
                      visible={visible}
                      connectTitle={p.title}
                      phoneCols={p.phoneCols}
                      precheck={p.connecting ?? null}
                      onCancelConnect={() => cancelPane(id)}
                      onRetryConnect={() => retryPane(id)}
                      onStatus={(s) => setStatus(id, s)}
                      onCwd={(cwd) => { if (cwd !== p.cwd) reportCwd(p.session, cwd); updatePane(id, (x) => (x.cwd === cwd ? {} : { cwd })); }}
                      onCommand={() => updatePane(id, (x) => ({ commands: x.commands + 1 }))}
                      onSession={(session) => onPaneSession(id, session)}
                      onCloseTab={() => dropPanes(tab.key, [id])}
                      closeLabel={multi ? t("terminal.closePane") : t("terminal.closeTab")}
                      onError={(e) => onPaneError(id, e)}
                      confirmPaste={(text) => new Promise((resolve) => setPending({ kind: "paste", text, resolve }))}
                      openLink={(url) => setPending({ kind: "link", url })}
                      cwd={p.cwd}
                      onSplit={(dir) => splitSame(tab.key, id, dir)}
                      canSplit={(dir) => canSplit(id, dir)}
                      fileDrop={fileDrop === id}
                      prediction={settings.terminal.commandPrediction && !(p.ai && !p.ai.takenOver) && !p.handover
                        && !(p.spec.kind === "server" && servers.find((x) => p.spec.kind === "server" && x.id === p.spec.serverId)?.noPrediction)}
                      broadcasting={inBroadcast}
                      onInput={inBroadcast ? (data) => onBroadcastInput(tab.key, id, data) : undefined}
                      onOutput={inBroadcast ? (tail) => onBroadcastOutput(tab.key, id, tail) : undefined}
                      onToggleBroadcast={multi ? () => toggleBroadcast(tab.key) : undefined}
                      onHandle={(h) => {
                        if (!h) { handles.current.delete(id); return; }
                        handles.current.set(id, h);
                        // 等菜单、侧栏这些触发打开的控件处理完自己的焦点再给终端；有对话框开着时不抢
                        if (focusOnMount.current.delete(id)) requestAnimationFrame(() => {
                          // 取最新的句柄：开发模式下组件会挂载两次，第一次的终端已经销毁
                          if (!document.activeElement?.closest("[role='dialog'], [role='alertdialog']")) handles.current.get(id)?.focus();
                        });
                      }}
                    />
                    </>
                    )}
                  </PaneBody>
                </div>
              );
        }}
      />

      {drag && (
        <>
          {drag.box && <div className={`pane-drop${drag.bad ? " pane-drop--bad" : ""}`} style={{ left: drag.box.left, top: drag.box.top, width: drag.box.width, height: drag.box.height }} aria-hidden="true" />}
          <div className={`drag-ghost${drag.drop ? " drag-ghost--ok" : ""}`} style={{ left: drag.x + 14, top: drag.y + 14 }} aria-hidden="true">
            <span className="pl-ellipsis">{drag.label}</span>
            {dropHint(drag) && <span className={drag.bad ? "pane-ghost__bad" : "drag-ghost__to"}>{dropHint(drag)}</span>}
          </div>
        </>
      )}
      <TransferCenter />
      {aiMenu && <Menu x={aiMenu.x} y={aiMenu.y} entries={aiMenu.entries} label={t("terminal.handover.button")} onClose={() => setAiMenu(null)} />}
      {splitMenu && <Menu x={splitMenu.x} y={splitMenu.y} entries={splitEntries(splitMenu)} label={t("terminal.splitChoose")} onClose={() => setSplitMenu(null)} />}

      {pending?.kind === "danger" && (
        <Dialog
          title={t("terminal.broadcast.dangerTitle")}
          danger
          onDismiss={() => setPending(null)}
          actions={<>
            <Button onClick={() => setPending(null)}>{t("common.cancel")}</Button>
            <Button variant="danger" onClick={() => {
              const { tab, rest, source } = pending;
              setPending(null);
              const buf = lineBuffers.current.get(tab);
              buf?.feed(rest);
              // 来源窗格的回车计数在它自己的终端里已经加过
              sendToBroadcast(tab, rest, source);
            }}>{t("terminal.broadcast.dangerRun", { count: broadcast[pending.tab]?.length ?? 0 })}</Button>
          </>}
        >
          {t("terminal.broadcast.dangerBody", { count: broadcast[pending.tab]?.length ?? 0 })}
          <pre className="mono pl-card" style={{ margin: "10px 0 0", fontSize: "var(--fs-xs)", whiteSpace: "pre-wrap" }}>{pending.line}</pre>
          <p style={{ margin: "8px 0 0", color: "var(--text-muted)", fontSize: "var(--fs-xs)" }}>{t("terminal.broadcast.dangerHint")}</p>
        </Dialog>
      )}
      {pending?.kind === "paste" && (
        <Dialog
          title={t("terminal.pasteConfirmTitle")}
          onDismiss={() => { pending.resolve(false); setPending(null); }}
          actions={<>
            <Button onClick={() => { pending.resolve(false); setPending(null); }}>{t("common.cancel")}</Button>
            <Button variant="primary" onClick={() => { pending.resolve(true); setPending(null); }}>{t("terminal.paste")}</Button>
          </>}
        >
          {t("terminal.pasteConfirmBody", { lines: pending.text.split(/\r?\n/).length })}
          <pre className="mono pl-card" style={{ maxHeight: 160, overflow: "auto", margin: "10px 0 0", fontSize: "var(--fs-xs)", whiteSpace: "pre-wrap" }}>{pending.text.slice(0, 2000)}</pre>
        </Dialog>
      )}
      {pending?.kind === "link" && (
        <Dialog
          title={t("terminal.openLinkTitle")}
          onDismiss={() => setPending(null)}
          actions={<>
            <Button onClick={() => setPending(null)}>{t("common.cancel")}</Button>
            <Button onClick={() => {
              const { url } = pending;
              void writeClipboardText(url).then(() => setPending(null), (e: unknown) => setPending({ kind: "link", url, error: formatError(e) }));
            }}>{t("terminal.copyLink")}</Button>
            <Button variant="primary" onClick={() => {
              const { url } = pending;
              void openInBrowser(url).then(() => setPending(null), (e: unknown) => setPending({ kind: "link", url, error: formatError(e) }));
            }}>{t("terminal.open")}</Button>
          </>}
        >
          {t("terminal.openLinkBody")}
          <div className="mono pl-card term-link__url">{pending.url}</div>
          {pending.error && <p className="term-link__error" role="alert">{pending.error}</p>}
        </Dialog>
      )}
      {pending?.kind === "hostkey" && (
        <HostKeyDialog step={pending.step} busy={pending.busy} onAccept={() => answerHostKey(true)} onCancel={() => answerHostKey(false)} />
      )}
      {pending?.kind === "checking" && (
        <Dialog
          title={t("terminal.connectingTo", { name: pending.conn.title })}
          onDismiss={closeQuick}
          actions={<Button onClick={closeQuick}>{t("common.cancel")}</Button>}
        >
          <span role="status">{t("terminal.checkingServer", { address: pending.conn.subtitle })}</span>
        </Dialog>
      )}
      {pending?.kind === "connectError" && (
        <Dialog
          title={t("terminal.connectFailed", { name: pending.conn.title })}
          danger
          onDismiss={closeQuick}
          actions={<>
            <Button onClick={closeQuick}>{t("common.cancel")}</Button>
            <Button variant="primary" onClick={() => connect(pending.conn)}>{t("common.retry")}</Button>
          </>}
        >
          <span role="alert" style={{ overflowWrap: "anywhere" }}>{pending.text}</span>
        </Dialog>
      )}
      {pending?.kind === "close" && closing && (
        <Dialog
          title={pending.pane ? t("terminal.closePaneTitle", { name: closing.first?.title ?? "" }) : t("terminal.closeTitle", { name: closing.first?.title ?? "" })}
          danger
          onDismiss={() => setPending(null)}
          actions={<>
            <Button onClick={() => setPending(null)}>{t("common.cancel")}</Button>
            <Button variant="danger" onClick={() => { setPending(null); dropPanes(pending.tab, closing.ids); }}>
              {pending.pane ? t("terminal.closePane") : t("terminal.closeTab")}
            </Button>
          </>}
        >
          {closing.ids.length > 1
            ? t("terminal.closeManyBody", { count: closing.ids.length })
            : pending.busy
              ? (closing.anyLocal ? t("terminal.closeBusyLocalBody") : t("terminal.closeBusyRemoteBody"))
              : (closing.anyLocal ? t("terminal.closeLocalBody") : t("terminal.closeRemoteBody"))}
        </Dialog>
      )}
      {pending?.kind === "quick" && (
        <Dialog
          title={t("terminal.quickConnect")}
          onDismiss={closeQuick}
          actions={<>
            <Button onClick={closeQuick}>{t("common.cancel")}</Button>
            <Button variant="primary" disabled={pending.busy} onClick={() => openQuick(pending.value, pending.target)}>
              {pending.busy ? t("terminal.checking") : t("terminal.connect")}
            </Button>
          </>}
        >
          <form onSubmit={(e) => { e.preventDefault(); if (!pending.busy) openQuick(pending.value, pending.target); }}>
            <TextField
              label={t("terminal.quickConnectHint")}
              autoFocus
              className="pl-input mono"
              value={pending.value}
              readOnly={pending.busy}
              onChange={(e) => setPending({ kind: "quick", value: e.target.value, error: null, target: pending.target })}
            />
            {pending.error && <div role="alert" style={{ color: "var(--danger)", marginTop: 8, fontSize: "var(--fs-sm)" }}>{pending.error}</div>}
          </form>
        </Dialog>
      )}
    </section>
  );
}

/**
 * 一个标签里的窗格：按布局算出的矩形绝对定位。每个窗格的组件始终挂在同一个 key 下，
 * 拆分、关闭、拖动分隔条都不会重建它（重建会断开终端会话）。
 */
/**
 * 所有标签的窗格放在同一层渲染（按窗格 ID 作 key，不按标签分组）：
 * 窗格在标签之间移动时组件不会被重建，终端会话保持。只显示当前标签的窗格和分隔条。
 */
function PaneStage({ tabs, activeTab, onResize, render }: {
  tabs: Tab[];
  activeTab: string | null;
  onResize: (tabKey: string, layout: Layout) => void;
  render: (paneId: string, tab: Tab) => ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [dragging, setDragging] = useState<Dir | null>(null);
  const current = tabs.find((x) => x.key === activeTab) ?? null;
  const all: { id: string; tab: Tab; rect: Rect }[] = [];
  for (const tab of tabs) {
    const { panes: rects } = computeRects(tab.layout);
    for (const id of Object.keys(rects)) all.push({ id, tab, rect: rects[id] });
  }
  // 窗格按 ID 排序渲染，布局变化、换标签时 DOM 顺序不变
  all.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
  const dividers = current ? computeRects(current.layout).dividers : [];

  const startDrag = (e: ReactPointerEvent<HTMLDivElement>, path: number[], index: number) => {
    if (e.button !== 0 || !current) return;
    e.preventDefault();
    const box = ref.current?.getBoundingClientRect();
    const layout = current.layout;
    const key = current.key;
    const span = pairSpan(layout, path, index);
    if (!box || !span) return;
    const el = e.currentTarget;
    el.setPointerCapture(e.pointerId);
    setDragging(span.dir);
    // 每个窗格至少 PANE_MIN 像素：两边子树里串联几个窗格，就至少要几倍（嵌套的窗格也不会被挤没）
    const size = span.dir === "row" ? box.width : box.height;
    const spanPx = Math.max(1, ((span.end - span.start) / 100) * size);
    const nodes = pairNodes(layout, path, index);
    const unit = PANE_MIN[span.dir];
    const minA = nodes ? (seriesCount(nodes[0], span.dir) * unit) / spanPx : 0.1;
    const minB = nodes ? (seriesCount(nodes[1], span.dir) * unit) / spanPx : 0.1;
    const move = (ev: PointerEvent) => {
      const pos = (span.dir === "row" ? (ev.clientX - box.left) / box.width : (ev.clientY - box.top) / box.height) * 100;
      onResize(key, resizeAt(layout, path, index, (pos - span.start) / (span.end - span.start), minA, minB));
    };
    const up = () => {
      setDragging(null);
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", up);
  };

  return (
    <div ref={ref} className={`pane-grid${dragging ? ` pane-grid--dragging-${dragging}` : ""}`} style={{ display: current ? "block" : "none" }}>
      {all.map(({ id, tab, rect: r }) => (
        <div key={id} className="pane-slot" data-pane={id} data-pane-tab={tab.key}
          style={{ left: `${r.x}%`, top: `${r.y}%`, width: `${r.w}%`, height: `${r.h}%`, display: tab.key === activeTab ? undefined : "none" }}>
          {render(id, tab)}
        </div>
      ))}
      {/* 外层的分隔条最后渲染，交叉处优先拖动外层 */}
      {[...dividers].sort((a, b) => b.path.length - a.path.length).map((d) => (
        <div
          key={`${activeTab}-${d.path.join(".")}-${d.index}`}
          className={`pane-divider pane-divider--${d.dir}`}
          style={d.dir === "row" ? { left: `${d.rect.x}%`, top: `${d.rect.y}%`, height: `${d.rect.h}%` } : { left: `${d.rect.x}%`, top: `${d.rect.y}%`, width: `${d.rect.w}%` }}
          role="separator"
          aria-orientation={d.dir === "row" ? "vertical" : "horizontal"}
          onPointerDown={(e) => startDrag(e, d.path, d.index)}
        />
      ))}
    </div>
  );
}

/**
 * 窗格内容：终端 + 右侧文件面板（可拖动调整宽度）。
 * 窗格太窄、两者放不下时，文件面板浮在终端上面（终端尺寸不变，不会被挤到看不清）。
 */
function PaneBody({ bar, files, onFilesWidth, panel, children }: {
  /** 终端上方的窗格标题栏（右边是按钮；放在终端那一侧，不压住文件面板） */
  bar: ReactNode;
  files: { open: boolean; width: number };
  onFilesWidth: (width: number) => void;
  panel: ReactNode;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(() => { if (el.clientWidth) setWidth(el.clientWidth); });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const overlay = width > 0 && width < FILES_MIN + TERM_MIN;
  const panelWidth = overlay
    ? Math.max(160, Math.min(width - 24, files.width))
    : Math.max(FILES_MIN, Math.min(files.width, width ? width - TERM_MIN : files.width));

  const startDrag = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    const box = ref.current?.getBoundingClientRect();
    if (!box) return;
    const el = e.currentTarget;
    el.setPointerCapture(e.pointerId);
    const move = (ev: PointerEvent) => onFilesWidth(Math.round(Math.max(FILES_MIN, Math.min(box.width - TERM_MIN, box.right - ev.clientX))));
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      el.removeEventListener("pointercancel", up);
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
    el.addEventListener("pointercancel", up);
  };

  return (
    <div ref={ref} className={`pane__body${panel && overlay ? " pane__body--overlay" : ""}`}>
      <div className="pane__term">{bar}{children}</div>
      {panel && (
        <div className="pane__files" style={{ width: panelWidth }}>
          {!overlay && <div className="pane__files-handle" role="separator" aria-orientation="vertical" onPointerDown={startDrag} />}
          {panel}
        </div>
      )}
    </div>
  );
}
