import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import RFB from "@novnc/novnc";
import { AppWindow, ClipboardPaste, Expand, Maximize, Minimize, Power, Unplug, X } from "lucide-react";
import { Button, Checkbox, Dialog, Select } from "../../components";
import { formatError, isAppError } from "../../errors";
import { readClipboardText, writeClipboardText } from "../shell/clipboard";
import { typingKeysyms } from "./keysyms";
import { openDesktopWindow } from "./popouts";
import { inTauri } from "../shell/TitleBar";
import { DesktopChannel } from "./channel";
import { desktopWindowUrl } from "./DesktopWindow";
import {
  acquireFailures, getDesktopApi,
  type AcquireSource, type DesktopConnection, type DesktopCreateProgress, type DesktopDetection, type DesktopSession,
} from "./api";
import "./desktop.css";

type Quality = "low" | "medium" | "high";
/** 画质三档：noVNC 的 JPEG 质量和压缩级别（压缩越高服务器越费 CPU） */
const QUALITY: Record<Quality, { quality: number; compression: number }> = {
  low: { quality: 3, compression: 6 },
  medium: { quality: 6, compression: 2 },
  high: { quality: 9, compression: 1 },
};

type Phase =
  | { kind: "detecting" }
  | { kind: "pick"; detection: DesktopDetection }
  | { kind: "connecting"; session: DesktopSession }
  | { kind: "password"; session: DesktopSession; error: string | null }
  | { kind: "connected"; session: DesktopSession; noAuth: boolean }
  | { kind: "closed"; session: DesktopSession; error: string | null }
  | { kind: "failed"; error: string }
  | { kind: "creating"; task: string; progress: DesktopCreateProgress | null }
  | { kind: "popped"; session: DesktopSession }
  | { kind: "createFailed"; request: CreateArgs; error: string; failures: { source: AcquireSource; text: string }[] };

/** 新建桌面要用的：检测结果（失败后重试、换来源要用）、桌面环境、获取方式 */
interface CreateArgs {
  detection: DesktopDetection;
  desktopId: string;
  allowOfficialDownload: boolean;
  manualPackage: string | null;
}

/** 官方包的下载地址（电脑上用浏览器下载后再“选择已下载的包”） */
const OFFICIAL_URL = "https://sourceforge.net/projects/tigervnc/files/stable/1.16.2/tigervnc-1.16.2.x86_64.tar.gz";

/** noVNC 1.7 的内部字段：恢复远程分辨率要用（公开接口只能“跟随窗口”，不能指定尺寸） */
interface RfbInternals {
  _fbWidth: number;
  _fbHeight: number;
  _supportsSetDesktopSize: boolean;
  _screenID: number;
  _screenFlags: number;
  _sock: unknown;
}
const rfbMessages = (RFB as unknown as { messages: { setDesktopSize(sock: unknown, width: number, height: number, id: number, flags: number): void } }).messages;

type Size = { w: number; h: number };
/** 把远程分辨率改成 size：发了 sent；已经是这个尺寸 same；服务器还没说支持改尺寸 unsupported */
function setRemoteSize(rfb: RFB, size: Size): "sent" | "same" | "unsupported" {
  const r = rfb as unknown as RfbInternals;
  if (!r._supportsSetDesktopSize) return "unsupported";
  if (r._fbWidth === size.w && r._fbHeight === size.h) return "same";
  rfbMessages.setDesktopSize(r._sock, size.w, size.h, r._screenID, r._screenFlags);
  return "sent";
}
/** 把远程分辨率改回 size；发了返回 true */
function restoreRemoteSize(rfb: RFB, size: Size | null): boolean {
  return !!size && setRemoteSize(rfb, size) === "sent";
}

/** 分辨率选项：跟随窗口、原始、常用预设（宽x高） */
const PRESETS = ["1280x720", "1366x768", "1600x900", "1920x1080", "2560x1440"];
/** 实际生效的分辨率：Pierlink 桌面没有“原始”这一说，默认跟随窗口 */
function effectiveResolution(resolution: string, pierlink: boolean): string {
  return pierlink && resolution === "original" ? "follow" : resolution;
}
function presetSize(resolution: string): Size | null {
  const m = /^(\d+)x(\d+)$/.exec(resolution);
  return m ? { w: Number(m[1]), h: Number(m[2]) } : null;
}

const LAST_KEY = "pierlink.desktop.last.";
const PREF_KEY = "pierlink.desktop.view";

/** resolution：follow 跟随窗口、original 原始、或预设的“宽x高” */
interface ViewPrefs { quality: Quality; fit: boolean; resolution: string }
function loadPrefs(): ViewPrefs {
  try {
    const v = JSON.parse(localStorage.getItem(PREF_KEY) ?? "{}") as Partial<ViewPrefs> & { follow?: boolean };
    const resolution = v.resolution === "follow" || v.resolution === "original" || PRESETS.includes(v.resolution ?? "")
      ? v.resolution!
      : v.follow ? "follow" : "original";
    return { quality: v.quality ?? "medium", fit: v.fit ?? true, resolution };
  } catch {
    return { quality: "medium", fit: true, resolution: "original" };
  }
}
function savePrefs(p: ViewPrefs) {
  try { localStorage.setItem(PREF_KEY, JSON.stringify(p)); } catch { /* 存不了就只在这次有效 */ }
}

/**
 * 服务器桌面窗格：检测服务器上的 VNC 桌面，选一个连接，用 noVNC 显示。
 * 数据经 SSH 走后端，本机和服务器都不开端口。
 */
export function DesktopPane({ serverId, visible, title, controls, windowTitle, standalone, onPoppedOut }: {
  serverId: string;
  visible: boolean;
  /** 独立窗口的标题（如“gpu-dev 桌面”）；不给就不显示“在独立窗口打开” */
  windowTitle?: string;
  /** 自己就是独立窗口 */
  standalone?: boolean;
  /** 窗格标题（可拖动换位置），放在工具栏最左边 */
  title?: ReactNode;
  /** 窗格自己的按钮（分屏、关闭），放在工具栏最右边 */
  controls?: ReactNode;
  /** 已经在独立窗口里打开：由上层关掉这个窗格（不给就留在这里显示“已在独立窗口打开”） */
  onPoppedOut?: () => void;
}) {
  const { t } = useTranslation();
  const [phase, setPhase] = useState<Phase>({ kind: "detecting" });
  const [prefs, setPrefs] = useState<ViewPrefs>(loadPrefs);
  const [size, setSize] = useState<string>("");
  const [fullscreen, setFullscreen] = useState(false);
  const [remember, setRemember] = useState(true);
  // 连接事件的回调在连接时就注册好了，要读“记住密码”的最新状态只能经 ref
  const rememberRef = useRef(true);
  rememberRef.current = remember;
  const [password, setPassword] = useState("");
  const rootRef = useRef<HTMLDivElement>(null);
  const screenRef = useRef<HTMLDivElement>(null);
  const rfbRef = useRef<RFB | null>(null);
  const connRef = useRef<DesktopConnection | null>(null);
  const usedSaved = useRef(false);
  /** 连上时服务器原本的分辨率：关掉“跟随窗口”、断开时改回去 */
  const originalSize = useRef<{ w: number; h: number } | null>(null);
  /** 远程分辨率是否被这里改过（跟随窗口或预设；Pierlink 桌面不算：那是我们自己的桌面）：断开时要改回去 */
  const followingRef = useRef(false);
  /** 选了预设、但服务器还没说支持改尺寸：等它说了再发 */
  const pendingSize = useRef<Size | null>(null);
  const [canResize, setCanResize] = useState(false);
  const typed = useRef<string | null>(null);
  /** 一行提示（如“断开后服务器可能结束这个桌面”），点 × 关掉 */
  const [notice, setNotice] = useState<string | null>(null);
  const [confirmStop, setConfirmStop] = useState(false);
  /** 全屏是这个窗格要的（页面全屏时只有它铺满窗口） */
  const wantFull = useRef(false);

  const disconnect = useCallback(() => {
    rfbRef.current?.disconnect();
    rfbRef.current = null;
    connRef.current?.close();
    connRef.current = null;
  }, []);

  const connect = useCallback(async (session: DesktopSession) => {
    disconnect();
    setPhase({ kind: "connecting", session });
    setSize("");
    const api = await getDesktopApi();
    const channel = new DesktopChannel();
    // 服务器那边断开的原因（noVNC 在已连接时收到关闭不算出错，原因要自己记下来显示）
    let closedReason = "";
    let conn: DesktopConnection;
    try {
      conn = await api.connect(serverId, session.target, (chunk) => channel.deliver(chunk), (reason) => { closedReason = reason; channel.remoteClosed(reason); });
    } catch (e) {
      setPhase({ kind: "closed", session, error: formatError(e) });
      return;
    }
    connRef.current = conn;
    channel.attach(conn);
    const saved = conn.probe.noAuth ? null : await api.savedPassword(serverId, session.key).catch(() => null);
    usedSaved.current = !!saved;
    typed.current = null;
    const screen = screenRef.current;
    if (!screen) { conn.close(); return; }
    const rfb = new RFB(screen, channel, { shared: true, credentials: saved ? { password: saved } : undefined });
    rfbRef.current = rfb;
    const p = loadPrefs();
    rfb.scaleViewport = p.fit;
    const mode = effectiveResolution(p.resolution, session.pierlink);
    rfb.resizeSession = mode === "follow";
    pendingSize.current = presetSize(mode);
    followingRef.current = mode !== "original" && !session.pierlink;
    rfb.qualityLevel = QUALITY[p.quality].quality;
    rfb.compressionLevel = QUALITY[p.quality].compression;
    rfb.background = "transparent";
    rfb.focusOnClick = true;
    // 远程程序把鼠标指针设成看不见时，显示一个小点，免得找不到鼠标在哪
    rfb.showDotCursor = true;
    rfb.addEventListener("connect", () => {
      // 此时还是服务器原本的尺寸（跟随窗口的改尺寸请求在这之后才发）
      const r = rfb as unknown as RfbInternals;
      originalSize.current = r._fbWidth > 0 ? { w: r._fbWidth, h: r._fbHeight } : null;
      setPhase({ kind: "connected", session, noAuth: conn.probe.noAuth });
      try { localStorage.setItem(LAST_KEY + serverId, session.key); } catch { /* 不记也行 */ }
      if (typed.current && rememberRef.current) void api.savePassword(serverId, session.key, typed.current).catch(() => {});
      typed.current = null;
      rfb.focus();
    });
    rfb.addEventListener("credentialsrequired", () => {
      setPassword("");
      setPhase({ kind: "password", session, error: null });
    });
    rfb.addEventListener("securityfailure", (e) => {
      const reason = (e as CustomEvent<{ reason?: string }>).detail?.reason;
      // 用的是记住的密码却错了：忘掉它，下次重新问
      if (usedSaved.current) void api.forgetPassword(serverId, session.key).catch(() => {});
      usedSaved.current = false;
      setPassword("");
      setPhase({ kind: "password", session, error: reason || t("desktop.wrongPassword") });
    });
    rfb.addEventListener("disconnect", (e) => {
      const clean = (e as CustomEvent<{ clean: boolean }>).detail?.clean;
      if (rfbRef.current === rfb) rfbRef.current = null;
      connRef.current = null;
      setPhase((cur) => (cur.kind === "password" || cur.kind === "popped" ? cur : { kind: "closed", session, error: closedReason || (clean ? null : t("desktop.lost")) }));
    });
    // 远程复制了东西：由后端写进 Windows 剪贴板（网页方式要求窗口在前台，远程在后台复制时会写不进去）
    rfb.addEventListener("clipboard", (e) => {
      const text = (e as CustomEvent<{ text: string }>).detail?.text;
      if (!text) return;
      void writeClipboardText(text).catch(() => {});
    });
  }, [disconnect, serverId, t]);

  // 检测：上次连过的或只有一个时直接连；auto 为 false（用户要换一个）时总是列出来让用户选
  const detect = useCallback(async (auto = true) => {
    setPhase({ kind: "detecting" });
    try {
      const detection = await (await getDesktopApi()).detect(serverId);
      let last: string | null = null;
      try { last = localStorage.getItem(LAST_KEY + serverId); } catch { /* 没有就算了 */ }
      const pick = !auto ? null : detection.sessions.find((s) => s.key === last) ?? (detection.sessions.length === 1 ? detection.sessions[0] : null);
      if (pick) void connect(pick);
      else setPhase({ kind: "pick", detection });
    } catch (e) {
      setPhase({ kind: "failed", error: formatError(e) });
    }
  }, [connect, serverId]);

  useEffect(() => {
    void detect();
    return () => {
      // 关标签时在跟随窗口：先把远程分辨率改回去，稍等发出去再断开
      if (followingRef.current && rfbRef.current && restoreRemoteSize(rfbRef.current, originalSize.current)) setTimeout(disconnect, 300);
      else disconnect();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [serverId]);

  // 新建 Pierlink 桌面：先获取 TigerVNC（需要时），再启动，然后直接连上
  const create = useCallback(async (args: CreateArgs) => {
    disconnect();
    const task = `${serverId}:${Date.now()}:${Math.random().toString(36).slice(2)}`;
    setPhase({ kind: "creating", task, progress: null });
    setNotice(null);
    // 远程分辨率按窗格大小（桌面会跟随窗口，这里只是初始值）
    const el = screenRef.current;
    const width = Math.min(2560, Math.max(640, Math.round(el?.clientWidth || 1280)));
    const height = Math.min(1600, Math.max(480, Math.round(el?.clientHeight || 800)));
    try {
      const api = await getDesktopApi();
      const created = await api.create(serverId, {
        task, desktopId: args.desktopId, width, height,
        allowOfficialDownload: args.allowOfficialDownload, manualPackage: args.manualPackage,
      }, (progress) => setPhase((cur) => (cur.kind === "creating" && cur.task === task ? { ...cur, progress } : cur)));
      if (created.mayEndOnLogout) setNotice(t("desktop.mayEnd"));
      void connect(created.session);
    } catch (e) {
      if (isAppError(e) && e.code === "vnc.cancelled") {
        void detect(false);
        return;
      }
      setPhase({
        kind: "createFailed", request: args, error: formatError(e),
        failures: acquireFailures(e).map((f) => ({ source: f.source, text: formatError(f.error) })),
      });
    }
  }, [connect, detect, disconnect, serverId, t]);

  const cancelCreate = () => {
    if (phase.kind === "creating") void getDesktopApi().then((api) => api.cancelCreate(phase.task));
  };

  const stopDesktop = async (session: DesktopSession) => {
    setConfirmStop(false);
    disconnect();
    if (session.display == null) return;
    try {
      await (await getDesktopApi()).stop(serverId, session.display);
      void detect(false);
    } catch (e) {
      setPhase({ kind: "closed", session, error: formatError(e) });
    }
  };

  // 远程分辨率：从画布读（noVNC 的画布尺寸就是远程桌面尺寸）
  useEffect(() => {
    if (phase.kind !== "connected") return;
    const read = () => {
      const c = screenRef.current?.querySelector("canvas");
      if (c && c.width && c.height) setSize(`${c.width}×${c.height}`);
      const rfb = rfbRef.current;
      const supports = !!rfb && (rfb as unknown as RfbInternals)._supportsSetDesktopSize;
      setCanResize(supports);
      if (rfb && supports && pendingSize.current) {
        setRemoteSize(rfb, pendingSize.current);
        pendingSize.current = null;
      }
    };
    read();
    const timer = setInterval(read, 1000);
    return () => clearInterval(timer);
  }, [phase.kind]);

  // 切回这个标签时把键盘交给远程桌面
  useEffect(() => {
    if (visible && phase.kind === "connected") rfbRef.current?.focus();
  }, [visible, phase.kind]);

  // 全屏：整个页面全屏、桌面铺满窗口。只让桌面这一块全屏时，noVNC 挂在页面上的鼠标指针和悬停提示都看不见
  useEffect(() => {
    const onChange = () => {
      if (!document.fullscreenElement) wantFull.current = false;
      setFullscreen(!!document.fullscreenElement && wantFull.current);
    };
    document.addEventListener("fullscreenchange", onChange);
    return () => {
      document.removeEventListener("fullscreenchange", onChange);
      if (wantFull.current && document.fullscreenElement) void document.exitFullscreen().catch(() => undefined);
    };
  }, []);
  const toggleFullscreen = () => {
    if (fullscreen) { void document.exitFullscreen().catch(() => undefined); return; }
    wantFull.current = true;
    void document.documentElement.requestFullscreen().catch(() => { wantFull.current = false; });
  };

  /**
   * 把 Windows 剪贴板的文字发到远程剪贴板。每次都发：重新连接后远程剪贴板是空的，
   * 远程复制过别的东西后也已经变了，不能按“上次发过同样的”跳过
   */
  const pushClipboard = useCallback(async () => {
    const text = await readClipboardText().catch(() => "");
    const rfb = rfbRef.current;
    if (!rfb || !text) return;
    rfb.clipboardPasteFrom(text);
  }, []);

  // 和 Windows 一样用 Ctrl+C、Ctrl+V 复制粘贴：在远程画面里按 Ctrl+V（或 Ctrl+Shift+V）时，
  // 先把 Windows 剪贴板发过去，再补发这个 V（按住的 Ctrl、Shift 已经由 noVNC 发给远程了）。
  // 只在按 Ctrl+V 时发：点进画面时不发，免得刚复制的口令之类一点画面就到了服务器上
  useEffect(() => {
    const el = screenRef.current;
    if (!el) return;
    /** Ctrl 现在是不是还按着：读剪贴板要等一会儿，补发 V 时 Ctrl 可能已经松开 */
    let ctrlHeld = false;
    const XK_CONTROL_L = 0xffe3;
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Control") ctrlHeld = ev.type === "keydown";
      if (ev.type !== "keydown" || ev.code !== "KeyV" || !ev.ctrlKey || ev.altKey || ev.metaKey || !rfbRef.current) return;
      ev.preventDefault();
      ev.stopPropagation();
      const shift = ev.shiftKey;
      void pushClipboard().finally(() => {
        const rfb = rfbRef.current;
        if (!rfb) return;
        if (ctrlHeld) { rfb.sendKey(shift ? 0x56 : 0x76, "KeyV"); return; }
        // 已经松开了 Ctrl：自己补一次 Ctrl+V，否则远程收到的只是一个 v
        rfb.sendKey(XK_CONTROL_L, "ControlLeft", true);
        rfb.sendKey(0x76, "KeyV");
        rfb.sendKey(XK_CONTROL_L, "ControlLeft", false);
      });
    };
    const released = () => { ctrlHeld = false; };
    // 在画面外松开 Ctrl（焦点还在这个窗口，不会触发 blur）、画面失去焦点时，也要当作松开了
    const onDocKeyUp = (ev: KeyboardEvent) => { if (ev.key === "Control") ctrlHeld = false; };
    el.addEventListener("keydown", onKey, true);
    el.addEventListener("keyup", onKey, true);
    el.addEventListener("focusout", released);
    document.addEventListener("keyup", onDocKeyUp, true);
    window.addEventListener("blur", released);
    return () => {
      el.removeEventListener("keydown", onKey, true);
      el.removeEventListener("keyup", onKey, true);
      el.removeEventListener("focusout", released);
      document.removeEventListener("keyup", onDocKeyUp, true);
      window.removeEventListener("blur", released);
    };
  }, [pushClipboard]);

  // 换分辨率：跟随窗口交给 noVNC；原始改回连上时的尺寸；预设直接改（服务器还没说支持时先记下）
  const applyResolution = (rfb: RFB, resolution: string, pierlink: boolean) => {
    const mode = effectiveResolution(resolution, pierlink);
    rfb.resizeSession = mode === "follow";
    pendingSize.current = null;
    const preset = presetSize(mode);
    if (mode === "original") restoreRemoteSize(rfb, originalSize.current);
    else if (preset && setRemoteSize(rfb, preset) === "unsupported") pendingSize.current = preset;
    followingRef.current = mode !== "original" && !pierlink;
  };

  // 独立窗口被直接关掉：先把被改过的远程分辨率改回去、断开，再关窗口（关窗口时页面来不及做清理）
  useEffect(() => {
    if (!standalone || !inTauri) return;
    let off: (() => void) | undefined;
    let alive = true;
    void import("@tauri-apps/api/window").then(({ getCurrentWindow }) => getCurrentWindow().onCloseRequested(async () => {
      const rfb = rfbRef.current;
      if (followingRef.current && rfb && restoreRemoteSize(rfb, originalSize.current)) await new Promise((r) => setTimeout(r, 300));
      followingRef.current = false;
      disconnect();
    })).then((u) => { if (alive) off = u; else u(); }, () => undefined);
    return () => { alive = false; off?.(); };
  }, [standalone, disconnect]);

  // 断开前把被改过的远程分辨率改回去，稍等发出去再断开
  const leave = () => {
    if (followingRef.current && rfbRef.current && restoreRemoteSize(rfbRef.current, originalSize.current)) setTimeout(disconnect, 300);
    else disconnect();
  };

  // 在独立窗口打开：新窗口自己连（自动连上次连的这个桌面），开出来后这里断开。
  // 不把远程分辨率改回去：新窗口接着用（跟随窗口时它会按自己的大小再改）
  const popOut = async () => {
    if (phase.kind !== "connected" || !windowTitle) return;
    const session = phase.session;
    let opened = false;
    try {
      opened = await openDesktopWindow(serverId, windowTitle, desktopWindowUrl(serverId, windowTitle));
    } catch (e) {
      // 后端的错误本身就是“没能打开独立窗口：原因”，不再套一层
      setNotice(isAppError(e) ? formatError(e) : t("desktop.popOutFailed", { reason: String(e) }));
      return;
    }
    if (!opened) return;
    followingRef.current = false;
    disconnect();
    if (onPoppedOut) onPoppedOut();
    else setPhase({ kind: "popped", session });
  };

  const updatePrefs = (patch: Partial<ViewPrefs>) => {
    const next = { ...prefs, ...patch };
    setPrefs(next);
    savePrefs(next);
    const rfb = rfbRef.current;
    if (!rfb) return;
    rfb.scaleViewport = next.fit;
    if (patch.resolution !== undefined) applyResolution(rfb, next.resolution, phase.kind === "connected" && phase.session.pierlink);
    rfb.qualityLevel = QUALITY[next.quality].quality;
    rfb.compressionLevel = QUALITY[next.quality].compression;
  };

  /** 把 Windows 剪贴板里的文字直接输入到远程（中文也能输），同时放进远程剪贴板；太长时只放进剪贴板 */
  const typeToRemote = async () => {
    const text = await readClipboardText().catch(() => "");
    const rfb = rfbRef.current;
    if (!rfb) return;
    rfb.focus();
    if (!text) return;
    rfb.clipboardPasteFrom(text);
    const keys = typingKeysyms(text);
    if (!keys) { setNotice(t("desktop.typeTooLong")); return; }
    for (const k of keys) rfb.sendKey(k, null);
  };

  const submitPassword = (e: React.FormEvent) => {
    e.preventDefault();
    if (phase.kind !== "password" || !password) return;
    typed.current = password;
    if (rfbRef.current) {
      rfbRef.current.sendCredentials({ password });
      setPhase({ kind: "connecting", session: phase.session });
    } else {
      // 上次握手已经结束（密码错）：重新连，用刚输的密码
      void (async () => {
        if (remember) await (await getDesktopApi()).savePassword(serverId, phase.session.key, password).catch(() => {});
        void connect(phase.session);
      })();
    }
  };

  const connected = phase.kind === "connected";
  const session = "session" in phase ? phase.session : null;
  return (
    <div className={`desk${fullscreen ? " desk--full" : ""}`} ref={rootRef}>
      <div className="desk__bar" role="toolbar" aria-label={t("desktop.title")}>
        {title}
        <span className="desk__status pl-ellipsis" role="status">
          {phase.kind === "connected"
            ? [t("desktop.connected"), size, phase.noAuth && !phase.session.pierlink ? t("desktop.noPassword") : ""].filter(Boolean).join(" · ")
            : phase.kind === "connecting" ? t("desktop.connecting")
              : phase.kind === "detecting" ? t("desktop.detecting")
                : phase.kind === "creating" ? t("desktop.creating") : ""}
        </span>
        {connected && (
          <>
            <Select label={t("desktop.quality")} value={prefs.quality} onChange={(v) => updatePrefs({ quality: v as Quality })}
              options={[{ value: "low", label: t("desktop.qualityLow") }, { value: "medium", label: t("desktop.qualityMedium") }, { value: "high", label: t("desktop.qualityHigh") }]} />
            <button type="button" className={`pane__btn${prefs.fit ? " pane__btn--on" : ""}`} aria-pressed={prefs.fit}
              title={prefs.fit ? t("desktop.actualSize") : t("desktop.fit")} aria-label={t("desktop.fit")} onClick={() => updatePrefs({ fit: !prefs.fit })}>
              <Expand size={14} />
            </button>
            <Select label={t("desktop.resolution")} disabled={!canResize} value={effectiveResolution(prefs.resolution, !!session?.pierlink)}
              onChange={(v) => updatePrefs({ resolution: v })}
              options={[
                { value: "follow", label: t("desktop.resFollow") },
                ...(session?.pierlink ? [] : [{ value: "original", label: t("desktop.resOriginal") }]),
                ...PRESETS.map((v) => ({ value: v, label: v.replace("x", "×") })),
              ]} />
            <button type="button" className="pane__btn" title={t("desktop.paste")} aria-label={t("desktop.paste")} onClick={() => void typeToRemote()}>
              <ClipboardPaste size={14} />
            </button>
            <button type="button" className="pane__btn" title={fullscreen ? t("desktop.exitFullscreen") : t("desktop.fullscreen")} aria-label={t("desktop.fullscreen")}
              onClick={toggleFullscreen}>
              {fullscreen ? <Minimize size={14} /> : <Maximize size={14} />}
            </button>
            <button type="button" className="pane__btn" title={t("desktop.disconnect")} aria-label={t("desktop.disconnect")} onClick={leave}>
              <Unplug size={14} />
            </button>
            {windowTitle && !standalone && (
              <button type="button" className="pane__btn" title={t("desktop.popOut")} aria-label={t("desktop.popOut")} onClick={() => void popOut()}>
                <AppWindow size={14} />
              </button>
            )}
            {session?.pierlink && session.display != null && (
              <button type="button" className="pane__btn" title={t("desktop.stop")} aria-label={t("desktop.stop")} onClick={() => setConfirmStop(true)}>
                <Power size={14} />
              </button>
            )}
          </>
        )}
        {controls}
      </div>
      <div className="desk__body">
        {/* noVNC 把画布放进这里；没连上时也留着，避免重建 */}
        <div className={`desk__screen${connected ? "" : " desk__screen--idle"}`} ref={screenRef} />
        {notice && (
          <div className="desk__note" role="status">
            <span>{notice}</span>
            <button type="button" className="pane__btn" aria-label={t("common.close")} onClick={() => setNotice(null)}><X size={13} /></button>
          </div>
        )}
        {phase.kind === "pick" && (
          <SessionList detection={phase.detection} onConnect={(s) => void connect(s)}
            onCreate={(desktopId) => void create({ detection: phase.detection, desktopId, allowOfficialDownload: false, manualPackage: null })} />
        )}
        {phase.kind === "creating" && (
          <div className="desk__card">
            <b>{t("desktop.creating")}</b>
            <CreateProgress progress={phase.progress} />
            <div className="desk__actions"><Button onClick={cancelCreate}>{t("common.cancel")}</Button></div>
          </div>
        )}
        {phase.kind === "createFailed" && (
          <CreateFailed state={phase} onRetry={(args) => void create(args)} onBack={() => void detect(false)} />
        )}
        {phase.kind === "password" && (
          <form className="desk__card" onSubmit={submitPassword}>
            <b>{phase.session.display != null ? t("desktop.passwordTitle", { display: phase.session.display }) : t("desktop.passwordTitlePlain")}</b>
            <input className="pl-input" type="password" autoFocus value={password} aria-label={t("desktop.password")} placeholder={t("desktop.password")}
              onChange={(e) => setPassword(e.target.value)} />
            <Checkbox label={t("desktop.remember")} checked={remember} onChange={(e) => setRemember(e.target.checked)} />
            <p className="desk__error" role="alert">{phase.error ?? ""}</p>
            <div className="desk__actions">
              <Button onClick={() => { disconnect(); setPhase({ kind: "closed", session: phase.session, error: null }); }}>{t("common.cancel")}</Button>
              <Button variant="primary" type="submit" disabled={!password}>{t("desktop.connect")}</Button>
            </div>
          </form>
        )}
        {phase.kind === "closed" && (
          <div className="desk__card">
            <b>{phase.error ? t("desktop.failedTitle") : t("desktop.closedTitle")}</b>
            <p className="desk__error" role={phase.error ? "alert" : undefined}>{phase.error ?? ""}</p>
            <div className="desk__actions">
              <Button onClick={() => void detect(false)}>{t("desktop.chooseOther")}</Button>
              <Button variant="primary" onClick={() => void connect(phase.session)}>{t("desktop.reconnect")}</Button>
            </div>
          </div>
        )}
        {phase.kind === "popped" && (
          <div className="desk__card">
            <b>{t("desktop.poppedTitle")}</b>
            <div className="desk__actions"><Button variant="primary" onClick={() => void connect(phase.session)}>{t("desktop.showHere")}</Button></div>
          </div>
        )}
        {phase.kind === "failed" && (
          <div className="desk__card">
            <b>{t("desktop.detectFailed")}</b>
            <p className="desk__error" role="alert">{phase.error}</p>
            <div className="desk__actions"><Button variant="primary" onClick={() => void detect()}>{t("common.retry")}</Button></div>
          </div>
        )}
      </div>
      {confirmStop && session && (
        <Dialog title={t("desktop.stopTitle")} danger onDismiss={() => setConfirmStop(false)}
          actions={<>
            <Button onClick={() => setConfirmStop(false)}>{t("common.cancel")}</Button>
            <Button variant="danger" onClick={() => void stopDesktop(session)}>{t("desktop.stop")}</Button>
          </>}>
          <p>{t("desktop.stopBody")}</p>
        </Dialog>
      )}
    </div>
  );
}

/** 新建的进度：当前在做哪一步；下载、上传时显示字节数和进度条 */
function CreateProgress({ progress }: { progress: DesktopCreateProgress | null }) {
  const { t } = useTranslation();
  const mb = (n: number) => `${(n / 1048576).toFixed(1)} MB`;
  let text = t("desktop.createPreparing");
  let percent: number | null = null;
  if (progress?.kind === "step") text = t(`desktop.source.${progress.source}`);
  else if (progress?.kind === "bytes") {
    const done = progress.total ? `${mb(progress.bytes)} / ${mb(progress.total)}` : mb(progress.bytes);
    text = t(progress.phase === "download" ? "desktop.downloading" : "desktop.uploading", { done });
    percent = progress.total ? Math.min(100, Math.round((progress.bytes / progress.total) * 100)) : null;
  } else if (progress?.kind === "ready" || progress?.kind === "starting") text = t("desktop.starting");
  return (
    <>
      <p role="status">{text}</p>
      <div className="desk__progress" aria-hidden={percent === null}>
        <span className={percent === null ? "desk__progress-busy" : undefined} style={percent === null ? undefined : { width: `${percent}%` }} />
      </div>
    </>
  );
}

/** 新建失败：每一步为什么不行；能用官方包时可以让这台电脑下载，或选已经下载好的包；有管理员权限可以用安装命令 */
function CreateFailed({ state, onRetry, onBack }: {
  state: Extract<Phase, { kind: "createFailed" }>;
  onRetry: (args: CreateArgs) => void;
  onBack: () => void;
}) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState<string | null>(null);
  const { request } = state;
  const { detection } = request;
  // 没让这台电脑下载、没选包时，那两步只是“没做”，不算失败（下面有对应按钮）；同样的原因只列一次
  const seen = new Set<string>();
  const failures = state.failures.filter((f) => {
    if (f.source === "LocalDownload" && !request.allowOfficialDownload) return false;
    if (f.source === "ManualPackage" && !request.manualPackage) return false;
    if (seen.has(f.text)) return false;
    seen.add(f.text);
    return true;
  });
  const acquireFailed = state.failures.length > 0;
  const copy = (text: string) => { void writeClipboardText(text).then(() => setCopied(text)); };
  const pick = async () => {
    const path = await (await getDesktopApi()).pickPackage().catch(() => null);
    if (path) onRetry({ ...request, manualPackage: path });
  };
  return (
    <div className="desk__card desk__card--wide">
      <b>{acquireFailed ? t("desktop.acquireFailedTitle") : t("desktop.createFailedTitle")}</b>
      {acquireFailed
        ? (
          <ul className="desk__reasons">
            {failures.map((f, i) => <li key={i}><span>{t(`desktop.source.${f.source}`)}</span><span className="desk__error">{f.text}</span></li>)}
          </ul>
        )
        : <p className="desk__error" role="alert">{state.error}</p>}
      {acquireFailed && detection.officialOk && (
        <>
          <p>{t("desktop.officialHint")}</p>
          <pre className="mono desk__cmd">{OFFICIAL_URL}</pre>
          <div className="desk__actions">
            <Button onClick={() => copy(OFFICIAL_URL)}>{copied === OFFICIAL_URL ? t("desktop.copied") : t("desktop.copyLink")}</Button>
            <Button onClick={() => void pick()}>{t("desktop.pickPackage")}</Button>
            {!request.allowOfficialDownload && (
              <Button variant="primary" onClick={() => onRetry({ ...request, allowOfficialDownload: true })}>{t("desktop.localDownload")}</Button>
            )}
          </div>
        </>
      )}
      {acquireFailed && detection.installCommand && (
        <>
          <p>{t("desktop.noneInstall")}</p>
          <pre className="mono desk__cmd">{detection.installCommand}</pre>
          <div className="desk__actions">
            <Button onClick={() => copy(detection.installCommand!)}>{copied === detection.installCommand ? t("desktop.copied") : t("desktop.copy")}</Button>
          </div>
        </>
      )}
      <div className="desk__actions">
        <Button onClick={onBack}>{t("desktop.back")}</Button>
        <Button variant="primary" onClick={() => onRetry(request)}>{t("common.retry")}</Button>
      </div>
    </div>
  );
}

/** 检测结果：已有的桌面逐个列出；能新建时给“新建桌面”；都不行时给安装命令 */
function SessionList({ detection, onConnect, onCreate }: {
  detection: DesktopDetection;
  onConnect: (s: DesktopSession) => void;
  onCreate: (desktopId: string) => void;
}) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const [env, setEnv] = useState(detection.desktops[0]?.id ?? "");
  const none = detection.sessions.length === 0;
  const createBox = detection.canCreate && (
    <div className="desk__create">
      <b>{none ? t("desktop.createFirstTitle") : t("desktop.createTitle")}</b>
      <p className="desk__hint">{t("desktop.createHint")}</p>
      <div className="desk__create-row">
        {detection.desktops.length > 1 && (
          <Select label={t("desktop.environment")} value={env} onChange={setEnv}
            options={detection.desktops.map((d) => ({ value: d.id, label: d.light ? t("desktop.lightName") : d.name }))} />
        )}
        <Button variant="primary" disabled={!env} onClick={() => onCreate(env)}>{t("desktop.create")}</Button>
      </div>
      {detection.desktops.length === 1 && detection.desktops[0].light
        ? <p className="desk__hint">{t("desktop.lightHint")}</p>
        : detection.needsServerProgram && <p className="desk__hint">{t("desktop.needsProgram")}</p>}
    </div>
  );
  if (none && !detection.canCreate) {
    const cmd = detection.installCommand;
    return (
      <div className="desk__card desk__card--wide">
        <b>{t("desktop.noneTitle")}</b>
        {detection.missingTools.length > 0 && <p>{t("desktop.missingTools", { tools: detection.missingTools.join(t("desktop.sep")) })}</p>}
        <p>{cmd ? t("desktop.noneInstall") : t("desktop.noneManual")}</p>
        {cmd && (
          <>
            <pre className="mono desk__cmd">{cmd}</pre>
            <div className="desk__actions">
              <Button onClick={() => { void writeClipboardText(cmd).then(() => setCopied(true)); }}>{copied ? t("desktop.copied") : t("desktop.copy")}</Button>
            </div>
          </>
        )}
      </div>
    );
  }
  return (
    <div className="desk__card desk__card--wide">
      {!none && (
        <>
          <b>{t("desktop.pickTitle")}</b>
          <ul className="desk__list">
            {detection.sessions.map((s) => (
              <li key={s.key} className="desk__row">
                <span className="desk__row-name">
                  <span className="mono">{s.display !== null ? `:${s.display}` : s.target.kind === "tcp" ? `:${s.target.port}` : "—"}</span>
                  <span>{s.pierlink && s.name ? s.name : t(`desktop.flavor.${s.flavor}`)}</span>
                  {s.pierlink && <span className="desk__tag">{t("desktop.tagPierlink")}</span>}
                  {s.exposed && <span className="desk__tag desk__tag--warn" title={t("desktop.exposedHint")}>{t("desktop.tagExposed")}</span>}
                </span>
                <Button size="sm" variant="primary" onClick={() => onConnect(s)}>{t("desktop.connect")}</Button>
              </li>
            ))}
          </ul>
        </>
      )}
      {createBox}
    </div>
  );
}
