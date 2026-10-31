import { useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { ArrowDown, ArrowLeft, Bot, ImagePlus, MoreHorizontal, ShieldAlert } from "lucide-react";
import { api, ApiError, type InputRequest, type Me, type Screen, type TerminalItem } from "./api";
import { handleError } from "./App";
import { parseSgrLine, stripSgr, type Style } from "./ansi";
import { detectChoices, type Choice } from "./choices";
import { prepareImage } from "./image";
import { FONT_SIZES, loadPrefs, savePrefs, type Prefs } from "./prefs";
import { useWakeLock } from "./viewport";

/** 键盘栏：手机键盘上没有的键（名字和电脑端接受的一致），常用的在前 */
const KEYS: { key: string; label: string; title?: string }[] = [
  { key: "Escape", label: "Esc" },
  { key: "Enter", label: "⏎" },
  { key: "ArrowUp", label: "↑" },
  { key: "ArrowDown", label: "↓" },
  { key: "ArrowLeft", label: "←" },
  { key: "ArrowRight", label: "→" },
  { key: "Tab", label: "Tab" },
  { key: "Shift+Tab", label: "⇧Tab" },
  { key: "Backspace", label: "⌫" },
  { key: "Ctrl+C", label: "^C" },
];

/** 读屏：平时要多少行；往上翻到顶时依次多要 */
const LINES_STEPS = [300, 1000, 2000];
/** 看着的时候多久读一次屏（毫秒） */
const POLL_MS = 1000;
/** 长按“发送”多久算长按（只输入不回车） */
const LONG_PRESS_MS = 450;
/** 按手机宽度显示：多久续一次期（电脑端 45 秒没收到就自动恢复） */
const FIT_RENEW_MS = 20_000;

/** 量出终端一格字的宽高（像素），按当前字号 */
function cellSize(screen: HTMLElement): { w: number; h: number } {
  const probe = document.createElement("span");
  probe.textContent = "0".repeat(40);
  probe.style.cssText = "position:absolute;visibility:hidden;white-space:pre;font:inherit;";
  screen.appendChild(probe);
  const w = probe.getBoundingClientRect().width / 40;
  probe.remove();
  const h = parseFloat(getComputedStyle(screen).lineHeight) || parseFloat(getComputedStyle(screen).fontSize) * 1.32;
  return { w: w || 7, h: h || 16 };
}

/** 屏幕区域放得下多少列、多少行（留出内边距和滚动条） */
function fitSize(screen: HTMLElement): { cols: number; rows: number } {
  const { w, h } = cellSize(screen);
  const style = getComputedStyle(screen);
  const width = screen.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
  const height = screen.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom);
  return { cols: Math.max(20, Math.min(300, Math.floor(width / w) - 1)), rows: Math.max(8, Math.min(100, Math.floor(height / h))) };
}

/** 终端：电脑上开着的终端列表，点开全屏看；有输入权限时可以打字、按键、发图片 */
export function TerminalsTab({ me, version, onAuthLost, pending, onOpenConfirms }: {
  me: Me;
  version: number;
  onAuthLost: () => void;
  /** 等着回答的确认数（终端页顶上提示） */
  pending: number;
  onOpenConfirms: () => void;
}) {
  const { t } = useTranslation();
  const [list, setList] = useState<TerminalItem[] | null>(null);
  const [open, setOpen] = useState<TerminalItem | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    api.terminals().then((l) => {
      if (!alive) return;
      setList(l);
      setError(null);
      // 正在看的终端被关掉了：回到列表
      setOpen((o) => (o && !l.some((x) => x.id === o.id) ? null : o));
    }, (e: unknown) => { if (alive) setError(handleError(e, onAuthLost)); });
    return () => { alive = false; };
  }, [version, onAuthLost]);

  if (open) {
    return (
      <TerminalView me={me} term={open} pending={pending} onOpenConfirms={() => { setOpen(null); onOpenConfirms(); }} onAuthLost={onAuthLost}
        onBack={(closed) => { setOpen(null); setNotice(closed ? t("mobile.terminals.closed") : null); }} />
    );
  }
  return (
    <div className="m-page">
      {notice && <p className="m-muted">{notice}</p>}
      {error && <p className="m-error">{error}</p>}
      {list === null && !error && <p className="m-muted">{t("common.loading")}</p>}
      {list?.length === 0 && <p className="m-muted">{t("mobile.terminals.none")}</p>}
      {list?.map((x) => (
        <button key={x.id} type="button" className="m-term-item" onClick={() => { setNotice(null); setOpen(x); }}>
          <span className="m-term-item__head">
            <span className="m-term-item__title">{x.title}</span>
            {x.ai && <span className="m-chip"><Bot size={12} aria-hidden="true" />AI</span>}
            {x.busy && <span className="m-chip m-chip--busy">{t("mobile.terminals.busy")}</span>}
          </span>
          <span className="m-term-item__sub">
            {x.kind === "windows" ? t("mobile.terminals.local") : x.server ?? ""}{x.cwd ? ` · ${x.cwd}` : ""}
          </span>
          {x.last && <span className="m-term-item__last">{x.last}</span>}
        </button>
      ))}
    </div>
  );
}

/** 一段带样式的文字 */
function styleOf(s: Style): CSSProperties | undefined {
  if (!s.fg && !s.bg && !s.bold && !s.dim && !s.italic && !s.underline && !s.inverse) return undefined;
  const fg = s.inverse ? s.bg ?? "var(--m-term-bg)" : s.fg;
  const bg = s.inverse ? s.fg ?? "var(--m-term-fg)" : s.bg;
  return {
    color: fg,
    background: bg,
    fontWeight: s.bold ? 700 : undefined,
    opacity: s.dim ? 0.62 : undefined,
    fontStyle: s.italic ? "italic" : undefined,
    textDecoration: s.underline ? "underline" : undefined,
  };
}

/** 一行：带颜色的拆成多段，没有颜色的直接是文字 */
function Line({ text, colored }: { text: string; colored: boolean }): ReactNode {
  if (!text) return <div className="m-line">{" "}</div>;
  if (!colored) return <div className="m-line">{text}</div>;
  return (
    <div className="m-line">
      {parseSgrLine(text).map((seg, i) => <span key={i} style={styleOf(seg.style)}>{seg.text}</span>)}
    </div>
  );
}

/** 复制文字：HTTP 页面没有剪贴板接口时退回 execCommand（iPhone 上要用选区选中，select() 对只读框不起作用） */
async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard && window.isSecureContext) { await navigator.clipboard.writeText(text); return true; }
  } catch { /* 下面再试 */ }
  const area = document.createElement("textarea");
  area.value = text;
  area.readOnly = true;
  area.contentEditable = "true";
  // 16px：iPhone 上小于 16px 的输入框获得焦点时会放大页面
  area.style.cssText = "position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;font-size:16px;";
  document.body.appendChild(area);
  let ok = false;
  try {
    const range = document.createRange();
    range.selectNodeContents(area);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
    area.setSelectionRange(0, text.length);
    ok = document.execCommand("copy");
    selection?.removeAllRanges();
  } catch {
    ok = false;
  }
  area.remove();
  return ok;
}

function TerminalView({ me, term, pending, onOpenConfirms, onBack, onAuthLost }: {
  me: Me;
  term: TerminalItem;
  pending: number;
  onOpenConfirms: () => void;
  /** closed：终端在电脑上已经关掉了 */
  onBack: (closed: boolean) => void;
  onAuthLost: () => void;
}) {
  const { t } = useTranslation();
  const [screen, setScreen] = useState<Screen | null>(null);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<{ text: string; ok?: boolean } | null>(null);
  /** 危险命令要确认：确认后接着发 queue 里剩下的；sent 是从输入框发出的文字（发完清空输入框） */
  const [danger, setDanger] = useState<{ lines: string[]; queue: InputRequest[]; sent?: string } | null>(null);
  const [prefs, setPrefs] = useState<Prefs>(loadPrefs);
  const [menu, setMenu] = useState(false);
  const [follow, setFollow] = useState(true);
  const [linesStep, setLinesStep] = useState(0);
  const [uploading, setUploading] = useState(false);
  const screenRef = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const composing = useRef(false);
  const fileRef = useRef<HTMLInputElement>(null);
  const lastKey = useRef("");
  /** 往上翻着的时候新读到的屏幕（先不显示，回到底部再换上）；hasNew 让“最新”按钮提示有新内容 */
  const held = useRef<Screen | null>(null);
  const [hasNew, setHasNew] = useState(false);
  /** 刚往上多要了行：下一次读到的要马上显示，并保持正在看的位置 */
  const loadingMore = useRef<{ height: number } | null>(null);
  useWakeLock(true);

  // 按手机宽度显示：电脑上的终端临时改成手机的列数，离开时恢复；电脑上有人在用时不改
  const sentSize = useRef<{ cols: number; rows: number } | null>(null);
  /** 重新量一次，尺寸变了就改（改字号后调用） */
  const refit = useRef<(() => void) | null>(null);
  useEffect(() => {
    const el = screenRef.current;
    if (!prefs.fit || !me.permissions.input || !el) return;
    let alive = true;
    let debounce: ReturnType<typeof setTimeout> | null = null;
    let warned = false;
    const send = (size: { cols: number; rows: number }) => {
      sentSize.current = size;
      api.size(term.id, size).then((ok) => {
        if (!ok && alive && !warned) { warned = true; setStatus({ text: t("mobile.terminals.fitBusy"), ok: true }); }
      }, () => { sentSize.current = null; });
    };
    const apply = (force: boolean) => {
      if (!alive || document.visibilityState !== "visible") return;
      const next = fitSize(el);
      const prev = sentSize.current;
      // 软键盘弹出时可见高度变小：列数没变就只增不减行数，免得每次点输入框电脑上的终端都改两次
      if (prev && prev.cols === next.cols) {
        if (!force && next.rows <= prev.rows + 1) return;
        next.rows = Math.max(prev.rows, next.rows);
      }
      send(next);
    };
    apply(true);
    refit.current = () => apply(false);
    // 续期：发上次的尺寸（这时键盘可能开着，重新量会把行数量小）
    const renew = setInterval(() => {
      if (document.visibilityState !== "visible") return;
      if (sentSize.current) send(sentSize.current);
      else apply(true);
    }, FIT_RENEW_MS);
    const ro = new ResizeObserver(() => {
      if (debounce) clearTimeout(debounce);
      debounce = setTimeout(() => apply(false), 600);
    });
    ro.observe(el);
    // 切到后台：恢复电脑上的尺寸；回来时再改
    const onVisible = () => {
      if (document.visibilityState === "visible") apply(true);
      else { sentSize.current = null; void api.size(term.id, null).catch(() => {}); }
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      alive = false;
      refit.current = null;
      clearInterval(renew);
      if (debounce) clearTimeout(debounce);
      ro.disconnect();
      document.removeEventListener("visibilitychange", onVisible);
      sentSize.current = null;
      void api.size(term.id, null).catch(() => {});
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [prefs.fit, term.id, me.permissions.input]);
  // 改字号：同一个终端里直接按新的格子大小再量（不先恢复电脑上的尺寸）
  useEffect(() => { refit.current?.(); }, [prefs.font]);

  // 看着、页面在前台时每秒读一次屏；内容没变不重新渲染
  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const load = () => {
      if (document.visibilityState !== "visible") { timer = setTimeout(load, POLL_MS); return; }
      api.screen(term.id, LINES_STEPS[linesStep]).then((s) => {
        if (!alive) return;
        const key = `${s.password}|${s.lines ? s.lines.join("\n") : s.text}`;
        if (key !== lastKey.current) {
          lastKey.current = key;
          // 往上翻着看的时候不换画面（只保留最近几百行，新输出会把正在看的内容往上挤）；多要行时除外
          if (!atBottom.current && !loadingMore.current) { held.current = s; setHasNew(true); }
          else { held.current = null; setScreen(s); }
        }
        setStatus((cur) => (cur && !cur.ok && cur.text === t("mobile.networkError") ? null : cur));
      }, (e: unknown) => {
        if (!alive) return;
        // 终端在电脑上关掉了：回到列表
        if (e instanceof ApiError && (e.status === 404 || e.code === "mobile.not_found")) { onBack(true); return; }
        const msg = handleError(e, onAuthLost);
        if (msg) setStatus({ text: msg });
      }).finally(() => { if (alive) timer = setTimeout(load, POLL_MS); });
    };
    load();
    const wake = () => { if (document.visibilityState === "visible" && timer) { clearTimeout(timer); load(); } };
    document.addEventListener("visibilitychange", wake);
    return () => { alive = false; if (timer) clearTimeout(timer); document.removeEventListener("visibilitychange", wake); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [term.id, linesStep, onAuthLost]);

  // 翻到顶多要过行：回到底部 30 秒后降回平时的行数，不一直每秒读两千行
  useEffect(() => {
    if (!follow || linesStep === 0) return;
    const timer = setTimeout(() => setLinesStep(0), 30_000);
    return () => clearTimeout(timer);
  }, [follow, linesStep]);

  const colored = !!screen?.lines;
  const rows = useMemo(() => (screen ? screen.lines ?? screen.text.split("\n") : []), [screen]);
  const plain = useMemo(() => (colored ? rows.map(stripSgr) : rows), [rows, colored]);
  const choices = useMemo(() => detectChoices(plain), [plain]);

  // 停在最底下时跟着新输出走；刚往上多要了行时，保持正在看的那一行在原位
  useLayoutEffect(() => {
    const el = screenRef.current;
    if (!el) return;
    if (loadingMore.current) {
      el.scrollTop += el.scrollHeight - loadingMore.current.height;
      loadingMore.current = null;
    } else if (atBottom.current) el.scrollTop = el.scrollHeight;
  }, [rows, prefs]);
  /** 回到底部：换上往上翻期间读到的新画面 */
  const showHeld = () => {
    if (held.current) { setScreen(held.current); held.current = null; }
    setHasNew(false);
  };
  const onScroll = () => {
    const el = screenRef.current;
    if (!el) return;
    const bottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 24;
    atBottom.current = bottom;
    setFollow(bottom);
    if (bottom && held.current) showHeld();
    // 翻到顶了：再多要一些行（读到后保持位置）
    if (el.scrollTop < 40 && !loadingMore.current && linesStep < LINES_STEPS.length - 1 && rows.length >= LINES_STEPS[linesStep] - 5) {
      loadingMore.current = { height: el.scrollHeight };
      setLinesStep((n) => n + 1);
    }
  };
  const jumpToEnd = () => {
    const el = screenRef.current;
    if (!el) return;
    atBottom.current = true;
    setFollow(true);
    showHeld();
    el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
  };

  /** 等危险确认时为 true：这期间排着队的按键不再发 */
  const dangerOpen = useRef(false);
  /**
   * 依次发出 queue（同一次操作，如多行文字和后面的回车）；遇到危险命令停下来问，
   * 确认后从这一条接着发，剩下的也算确认过（同样的内容不再问第二次）
   */
  const run = async (queue: InputRequest[], sent?: string) => {
    setBusy(true);
    setStatus(null);
    try {
      for (let i = 0; i < queue.length; i++) {
        try {
          await api.input(term.id, queue[i]);
        } catch (e) {
          if (e instanceof ApiError && e.status === 409 && e.body?.needs_confirm) {
            dangerOpen.current = true;
            setDanger({ lines: Array.isArray(e.body.lines) ? (e.body.lines as string[]) : [], queue: queue.slice(i).map((q) => ({ ...q, confirmed: true })), sent });
            return false;
          }
          throw e;
        }
      }
      dangerOpen.current = false;
      setDanger(null);
      // 发出去了：清空输入框（发送期间又改过就不动）
      if (sent !== undefined) setText((cur) => (cur.replace(/\r\n?/g, "\n") === sent ? "" : cur));
      return true;
    } catch (e) {
      const msg = handleError(e, onAuthLost);
      if (msg) setStatus({ text: msg });
      return false;
    } finally {
      setBusy(false);
    }
  };
  /** 文字、按键、选项排队依次发（网络慢时连按两下 ↓ 不丢第二下，也不会跑到前一句前面）；等危险确认时不发 */
  const chain = useRef<Promise<unknown>>(Promise.resolve());
  const enqueue = (queue: InputRequest[], sent?: string): Promise<boolean> => {
    const next = chain.current.then(() => (dangerOpen.current ? false : run(queue, sent)));
    chain.current = next.catch(() => {});
    return next;
  };
  const closeDanger = () => { dangerOpen.current = false; setDanger(null); };
  /** 发出输入框里的文字；enter 为 false 时只输入不回车。多行按粘贴写入，不会一行一行地提交 */
  const sendText = async (enter: boolean) => {
    const value = text.replace(/\r\n?/g, "\n");
    if (!value) { if (enter) await enqueue([{ keys: ["Enter"] }]); return; }
    // 多行：先按粘贴写进去，回车单独发（同一次写进去的回车会被 Claude Code 当成粘贴内容里的换行）
    const queue: InputRequest[] = value.includes("\n")
      ? [{ text: value, paste: true }, ...(enter ? [{ keys: ["Enter"] }] : [])]
      : [{ text: enter ? `${value}\r` : value }];
    await enqueue(queue, value);
  };
  const press = (key: string) => void enqueue([{ keys: [key] }]);
  const choose = (c: Choice) => void enqueue([{ text: c.enter ? `${c.key}\r` : c.key }]);

  // 长按“发送”只输入不回车
  const longTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const longFired = useRef(false);
  /** 按下“发送”时输入框有焦点（键盘开着）：发完把焦点还给它 */
  const typing = useRef(false);
  const sendDown = (e: ReactPointerEvent) => {
    // 不让输入框失焦：发完一句键盘还在，接着打字
    e.preventDefault();
    typing.current = document.activeElement === textRef.current;
    longFired.current = false;
    longTimer.current = setTimeout(() => {
      longFired.current = true;
      navigator.vibrate?.(20);
      void sendText(false);
    }, LONG_PRESS_MS);
  };
  const sendUp = () => {
    if (longTimer.current) clearTimeout(longTimer.current);
    if (!longFired.current) void sendText(true);
    // 有的浏览器按按钮时还是会让输入框失焦：在这次点按里放回去，键盘不会收起再弹出
    if (typing.current && document.activeElement !== textRef.current) textRef.current?.focus();
  };
  const sendCancel = () => { if (longTimer.current) clearTimeout(longTimer.current); longFired.current = true; };

  const pickImage = async (file: File | undefined) => {
    if (!file) return;
    setUploading(true);
    setStatus({ text: t("mobile.terminals.imageSending"), ok: true });
    try {
      await api.image(term.id, await prepareImage(file));
      setStatus({ text: t("mobile.terminals.imageSent"), ok: true });
    } catch (e) {
      const msg = e instanceof ApiError ? handleError(e, onAuthLost) : t("mobile.terminals.imageFailed");
      if (msg) setStatus({ text: msg });
    } finally {
      setUploading(false);
      if (fileRef.current) fileRef.current.value = "";
    }
  };

  const updatePrefs = (patch: Partial<Prefs>) => setPrefs((p) => { const next = { ...p, ...patch }; savePrefs(next); return next; });
  const canType = me.permissions.input && !screen?.password;
  const textRef = useRef<HTMLTextAreaElement>(null);
  // 输入框随内容长高（最多几行，再多就在框里滚）
  useLayoutEffect(() => {
    const el = textRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
  }, [text]);

  return (
    <div className="m-term" style={{ "--m-font": `${FONT_SIZES[prefs.font]}px` } as CSSProperties}>
      <div className="m-term__bar">
        <button type="button" className="m-icon-btn" aria-label={t("mobile.back")} onClick={() => onBack(false)}><ArrowLeft size={20} /></button>
        <span className="m-term__title">
          <span className="m-term__name">{term.title}</span>
          <span className="m-term__where">{term.kind === "windows" ? t("mobile.terminals.local") : term.server ?? ""}</span>
        </span>
        <button type="button" className="m-icon-btn" aria-label={t("mobile.terminals.more")} aria-expanded={menu} onClick={() => setMenu((v) => !v)}><MoreHorizontal size={20} /></button>
      </div>
      {menu && (
        <div className="m-term__menu" role="menu">
          <button type="button" role="menuitem" className="m-btn m-btn--small" disabled={prefs.font === 0} onClick={() => updatePrefs({ font: prefs.font - 1 })}>A−</button>
          <button type="button" role="menuitem" className="m-btn m-btn--small" disabled={prefs.font === FONT_SIZES.length - 1} onClick={() => updatePrefs({ font: prefs.font + 1 })}>A+</button>
          <button type="button" role="menuitemcheckbox" aria-checked={prefs.wrap} className={`m-btn m-btn--small${prefs.wrap ? " m-btn--on" : ""}`} onClick={() => updatePrefs({ wrap: !prefs.wrap })}>{t("mobile.terminals.wrap")}</button>
          {me.permissions.input && (
            <button type="button" role="menuitemcheckbox" aria-checked={prefs.fit} className={`m-btn m-btn--small${prefs.fit ? " m-btn--on" : ""}`} onClick={() => updatePrefs({ fit: !prefs.fit })}>{t("mobile.terminals.fit")}</button>
          )}
          <button type="button" role="menuitem" className="m-btn m-btn--small" onClick={() => void copyText(plain.join("\n")).then((ok) => { setStatus({ text: ok ? t("mobile.terminals.copied") : t("mobile.terminals.copyFailed"), ok }); setMenu(false); })}>{t("mobile.terminals.copy")}</button>
        </div>
      )}
      {pending > 0 && (
        <button type="button" className="m-term__alert" onClick={onOpenConfirms}>
          <ShieldAlert size={16} aria-hidden="true" />{t("mobile.terminals.confirmBanner", { count: pending })}
        </button>
      )}
      <div className="m-term__body">
        <div ref={screenRef} className={`m-screen${prefs.wrap ? " m-screen--wrap" : ""}`} onScroll={onScroll} aria-live="off">
          {screen ? rows.map((line, i) => <Line key={i} text={line} colored={colored} />) : <div className="m-line m-muted">{t("common.loading")}</div>}
        </div>
        {!follow && (
          <button type="button" className={`m-jump${hasNew ? " m-jump--new" : ""}`} onClick={jumpToEnd}><ArrowDown size={16} aria-hidden="true" />{t(hasNew ? "mobile.terminals.jumpNew" : "mobile.terminals.jump")}</button>
        )}
      </div>
      {screen?.password && <p className="m-warn">{t("mobile.terminals.password")}</p>}
      {danger && (
        <div className="m-sheet" role="alertdialog">
          <p><b>{t("mobile.terminals.dangerTitle")}</b></p>
          {danger.lines.length > 0 && <pre className="m-code">{danger.lines.join("\n")}</pre>}
          <div className="m-actions">
            <button type="button" className="m-btn" onClick={closeDanger}>{t("common.cancel")}</button>
            <button type="button" className="m-btn m-btn--danger" disabled={busy} onClick={() => void run(danger.queue, danger.sent)}>{t("mobile.terminals.runAnyway")}</button>
          </div>
        </div>
      )}
      {me.permissions.input ? (
        <div className="m-term__input">
          {choices && canType && (
            <div className="m-choices" role="group" aria-label={t("mobile.terminals.choices")}>
              {choices.map((c) => (
                <button key={c.key} type="button" className={`m-choice${c.current ? " m-choice--current" : ""}`} onClick={() => choose(c)}>
                  <span className="m-choice__key">{c.key}</span><span className="m-choice__label">{c.label}</span>
                </button>
              ))}
            </div>
          )}
          <div className="m-keys">
            {KEYS.map((k) => <button key={k.key} type="button" className="m-key" disabled={!canType} onClick={() => press(k.key)}>{k.label}</button>)}
          </div>
          <div className="m-term__line">
            <button type="button" className="m-icon-btn m-icon-btn--field" aria-label={t("mobile.terminals.image")} disabled={!canType || uploading} onClick={() => fileRef.current?.click()}><ImagePlus size={20} /></button>
            <input ref={fileRef} type="file" accept="image/*" hidden onChange={(e) => void pickImage(e.target.files?.[0])} />
            <textarea
              ref={textRef}
              rows={1}
              value={text}
              disabled={!canType}
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              placeholder={canType ? t("mobile.terminals.placeholder") : ""}
              onChange={(e) => setText(e.target.value)}
              onCompositionStart={() => { composing.current = true; }}
              onCompositionEnd={() => { composing.current = false; }}
              onKeyDown={(e) => {
                // 电脑浏览器里（精确指针）Enter 发送、Shift+Enter 换行；手机上的回车照常换行，点“发送”发出
                if (e.key !== "Enter" || e.shiftKey || composing.current || e.nativeEvent.isComposing) return;
                if (!window.matchMedia("(pointer: fine)").matches) return;
                e.preventDefault();
                void sendText(true);
              }}
            />
            <button type="button" className="m-btn m-btn--primary m-send" disabled={busy || !canType} title={t("mobile.terminals.sendHint")}
              onPointerDown={sendDown} onPointerUp={sendUp} onPointerLeave={sendCancel} onPointerCancel={sendCancel}
              onContextMenu={(e) => e.preventDefault()}
              onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); void sendText(true); } }}>
              {t("mobile.terminals.send")}
            </button>
          </div>
        </div>
      ) : (
        <p className="m-muted m-term__noinput">{t("mobile.terminals.noInput")}</p>
      )}
      <p className={`m-status${status?.ok ? " m-status--ok" : ""}`} role="status">{status?.text ?? ""}</p>
    </div>
  );
}
