import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { Bot, CircleAlert, Pin } from "lucide-react";
import { Button } from "../../components";
import { getServersApi } from "../servers/api";
import { getMcpApi, type McpActivity, type McpComputer, type McpNotice, type McpSession } from "./api";
import "./mcp.css";

/** 需要用户处理的提示（要登录、AI 终端在等密码） */
type Attention = { key: string; text: string };

/** 距现在多久：刚刚 / N 分钟前 / N 小时前 */
export function agoText(t: (key: string, o?: Record<string, unknown>) => string, ms: number, now: number): string {
  const min = Math.floor(Math.max(0, now - ms) / 60000);
  if (min < 1) return t("mcp.ago.now");
  if (min < 60) return t("mcp.ago.minutes", { count: min });
  return t("mcp.ago.hours", { count: Math.floor(min / 60) });
}

/**
 * 标题栏上的 AI 接入状态：有 AI 通过 Pierlink 操作这台电脑或服务器、或有需要处理的提示时出现。
 * 文字直接说状态（“AI 正在操作”“AI 已连接”），不显示连接数；点开看是哪个 AI、在做什么，可以全部停止。
 */
export function McpIndicator() {
  const { t } = useTranslation();
  const [sessions, setSessions] = useState<McpSession[]>([]);
  const [activity, setActivity] = useState<McpActivity[]>([]);
  /** 点开看输出的那一条 */
  const [openAct, setOpenAct] = useState<string | null>(null);
  const [attention, setAttention] = useState<Attention[]>([]);
  const [names, setNames] = useState<Record<string, string>>({});
  const [open, setOpen] = useState(false);
  const [stopping, setStopping] = useState(false);
  // 弹出框开着时每 30 秒刷新一次“几分钟前”
  const [now, setNow] = useState(() => Date.now());
  // 最近两分钟有过操作：服务器入口的无握手客户端两次请求之间没有会话，也要让标题栏一直能看到
  const [recent, setRecent] = useState(false);
  const recentTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const btn = useRef<HTMLButtonElement>(null);
  const refreshRef = useRef<() => void>(() => {});
  const pop = useRef<HTMLDivElement>(null);
  const namesRef = useRef(names);
  namesRef.current = names;
  // 用户固定了 AI 操作的电脑（几台电脑同时接入时）：按钮一直显示，写明是哪台
  const [running, setRunning] = useState(0);
  const [pinned, setPinned] = useState<McpComputer | null>(null);
  useEffect(() => {
    if (!running) { setPinned(null); return; }
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | null = null;
    // 要到服务器上读：只有这一台电脑时十分钟查一次，有几台或固定了一台时每分钟查一次
    const load = () => {
      if (timer) clearTimeout(timer);
      void getMcpApi().then((api) => api.computers()).then((c) => {
        if (!alive) return;
        setPinned(c.pinned);
        timer = setTimeout(load, c.computers.length > 1 || c.pinned ? 60000 : 600000);
      }, () => { if (alive) timer = setTimeout(load, 600000); });
    };
    load();
    let off: (() => void) | null = null;
    void getMcpApi().then((api) => api.onPin(load)).then((u) => { if (alive) off = u; else u(); });
    return () => { alive = false; if (timer) clearTimeout(timer); off?.(); };
  }, [running]);
  const unpin = async () => {
    setPinned(null);
    try { await (await getMcpApi()).setPin(null, ""); } catch (e) { console.error(e); }
  };

  const serverName = (id: string) => (id === "windows" ? t("mcp.targetWindows") : namesRef.current[id] ?? id);
  /** 连接从哪个入口来：本机入口写“这台电脑”，服务器入口写服务器名 */
  const entryName = (id: string) => (id === "windows" ? t("mcp.entryWindows") : namesRef.current[id] ?? id);

  useEffect(() => {
    let alive = true;
    const offs: (() => void)[] = [];
    const keep = (u: () => void) => { if (alive) offs.push(u); else u(); };
    const load = async () => {
      const api = await getMcpApi();
      try {
        const s = await api.status();
        if (alive) { setSessions(s.sessions); setRunning(s.entries.filter((e) => e.state === "running").length); }
      } catch { /* 后端还没准备好：下次事件再取 */ }
    };
    // 一连串调用会连着发事件：合并成一次读取
    let soon: ReturnType<typeof setTimeout> | null = null;
    const refresh = () => {
      if (soon) clearTimeout(soon);
      soon = setTimeout(() => { soon = null; void load(); }, 150);
    };
    refreshRef.current = refresh;
    const noticeText = (n: McpNotice): Attention | null =>
      n.kind === "loginNeeded" ? { key: `login-${n.serverId}`, text: t("mcp.noticeLogin", { server: serverName(n.serverId) }) }
        : n.kind === "passwordPrompt" ? { key: `pw-${n.terminalId}`, text: t("mcp.noticePassword") }
          : null;
    void getMcpApi().then((api) => {
      void api.onSessions(() => void refresh()).then(keep);
      void api.onActivity((a) => {
        setActivity((list) => [a, ...list].slice(0, 30));
        // 入口关着、调用被拒绝的不算（AI 客户端还开着、还在试着调用时，不显示“AI 已连接”）
        if (a.outcome === "mcp.entry_disabled") return;
        setRecent(true);
        if (recentTimer.current) clearTimeout(recentTimer.current);
        recentTimer.current = setTimeout(() => setRecent(false), 120000);
      }).then(keep);
      void api.onNotice((n) => {
        const item = noticeText(n);
        if (item) setAttention((list) => [item, ...list.filter((x) => x.key !== item.key)].slice(0, 5));
      }).then(keep);
      void refresh();
    });
    void getServersApi().then((api) => api.list()).then(
      (list) => { if (alive) setNames(Object.fromEntries(list.map((s) => [s.id, s.name]))); },
      () => {},
    );
    return () => { alive = false; offs.forEach((u) => u()); if (recentTimer.current) clearTimeout(recentTimer.current); if (soon) clearTimeout(soon); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!open) return;
    setNow(Date.now());
    const tick = setInterval(() => setNow(Date.now()), 30000);
    void getMcpApi().then((api) => api.activity(30)).then(setActivity, () => {});
    const onDown = (e: MouseEvent) => {
      const target = e.target as Node;
      // 在停留的提示框里选文字不算点到外面
      if (target instanceof Element && target.closest(".pl-tip")) return;
      if (!pop.current?.contains(target) && !btn.current?.contains(target)) setOpen(false);
    };
    // 停留的提示框开着时 Esc 只关提示框
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape" && !document.querySelector(".pl-tip--hold")) setOpen(false); };
    window.addEventListener("mousedown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    return () => {
      clearInterval(tick);
      window.removeEventListener("mousedown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open]);

  // 显示“正在操作”时每 1.2 秒复查一次：很快就结束的调用不一定有结束的事件
  const anyBusy = sessions.some((s) => s.busy);
  useEffect(() => {
    if (!anyBusy) return;
    const t = setInterval(() => refreshRef.current(), 1200);
    return () => clearInterval(t);
  }, [anyBusy]);

  if (!sessions.length && !attention.length && !recent && !open && !pinned) return null;

  const stopAll = async () => {
    setStopping(true);
    try {
      await (await getMcpApi()).stopAll();
    } catch (e) {
      console.error(e);
    } finally {
      setStopping(false);
    }
  };
  const r = btn.current?.getBoundingClientRect();
  const time = (ms: number) => new Date(ms).toLocaleTimeString();
  const busy = sessions.some((s) => s.busy);
  // 状态直接写在按钮上：正在操作 / 已连接（最近有操作也算）/ 只有要处理的提示
  // 固定了电脑时状态文字不变（标题栏不挪），只加一个图钉；没有别的状态可说时才写“AI 固定操作 · X”
  const label = busy ? t("mcp.indicatorBusy")
    : sessions.length || recent ? t("mcp.indicatorConnected")
      : attention.length ? t("mcp.indicatorAttention")
        : pinned ? t("mcp.indicatorPinned", { computer: pinned.name })
          : t("mcp.indicatorAttention");
  const pinText = pinned ? t("mcp.indicatorPinned", { computer: pinned.name }) : null;

  return (
    <>
      <button
        ref={btn}
        type="button"
        className={`xfer-btn mcp-btn${sessions.length || busy ? " xfer-btn--busy" : ""}`}
        aria-expanded={open}
        aria-haspopup="dialog"
        aria-label={label}
        title={pinText ? `${pinText}\n${t("mcp.indicatorHint")}` : t("mcp.indicatorHint")}
        onClick={() => { setOpen(!open); if (!open) setAttention([]); }}
      >
        {busy ? <span className="mcp-btn__pulse" aria-hidden="true" /> : <Bot size={14} />}
        <span className="xfer-btn__text">{label}</span>
        {pinned && (sessions.length > 0 || recent || attention.length > 0) && <Pin size={12} aria-hidden="true" />}
        {attention.length > 0 && <span className="xfer-btn__dot" aria-hidden="true" />}
      </button>
      {open && r && createPortal(
        <div ref={pop} className="xfer-pop mcp-pop" role="dialog" aria-label={t("mcp.indicator")} style={{ top: r.bottom + 6, right: Math.max(8, window.innerWidth - r.right) }}>
          <div className="xfer-pop__head">
            <span className="xfer-pop__title">{t("mcp.indicator")}</span>
            {/* 最近有操作也给按钮：服务器入口的无握手客户端两次请求之间没有会话，被拒绝的决定也要靠它重置 */}
            {(sessions.length > 0 || recent) && <Button size="sm" variant="danger-ghost" disabled={stopping} onClick={() => void stopAll()}>{t("mcp.stopAll")}</Button>}
          </div>
          <div className="mcp-pop__body" data-pl-tip-hold="">
            <div className="mcp-pop__hint">{t("mcp.popHint")}</div>
            {pinned && (
              <div className="mcp-pop__pinned">
                <span className="pl-ellipsis">{t("mcp.indicatorPinned", { computer: pinned.name })}</span>
                <Button size="sm" variant="ghost" onClick={() => void unpin()}>{t("mcp.pinCancel")}</Button>
              </div>
            )}
            {attention.map((a) => <div key={a.key} className="mcp-pop__attention"><CircleAlert size={13} />{a.text}</div>)}
            <div className="mcp-pop__section">{t("mcp.sessions")}</div>
            {sessions.length === 0
              ? <div className="xfer-pop__empty">{t("mcp.noSessions")}</div>
              : sessions.map((s) => (
                <div key={s.id} className="mcp-pop__session">
                  <span className="pl-ellipsis">{s.client || t("mcp.unknownClient")} · {entryName(s.entry)}</span>
                  <span className={`mcp-pop__state${s.busy ? " mcp-pop__state--busy" : ""}`}>{s.busy ? t("mcp.busyNow") : agoText(t, s.lastActiveMs, now)}</span>
                </div>
              ))}
            <div className="mcp-pop__section">{t("mcp.recent")}</div>
            {activity.length === 0
              ? <div className="xfer-pop__empty">{t("mcp.noActivity")}</div>
              : activity.map((a, i) => {
                const key = `${a.timeMs}-${i}`;
                const out = a.output && (a.output.stdout || a.output.stderr) ? a.output : null;
                return (
                <div key={key}>
                <div className={`mcp-pop__act mcp-pop__act--${a.outcome === "ok" ? "ok" : "bad"}${out ? " mcp-pop__act--out" : ""}`} title={out ? t("mcp.showOutput") : a.summary}
                  role={out ? "button" : undefined} tabIndex={out ? 0 : undefined} aria-expanded={out ? openAct === key : undefined}
                  onClick={out ? () => setOpenAct(openAct === key ? null : key) : undefined}
                  onKeyDown={out ? (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); setOpenAct(openAct === key ? null : key); } } : undefined}>
                  <span className="mcp-pop__time">{time(a.timeMs)}</span>
                  <span className="mcp-pop__tool mono">{a.tool}</span>
                  <span className="pl-ellipsis">{a.forwardedTo ? `${t("mcp.forwardedTo", { computer: a.forwardedTo })} ` : ""}{a.target ? `${serverName(a.target)}: ` : ""}{a.summary}</span>
                  {a.outcome === "ok" && (a.approval === "session" || a.approval === "auto") && <span className="mcp-pop__outcome mcp-pop__outcome--auto" title={t(`mcp.approvedBy.${a.approval}`)}>{t(`mcp.approvedBy.${a.approval}`)}</span>}
                  {a.outcome !== "ok" && <span className="mcp-pop__outcome" title={a.outcome}>{a.outcome === "denied" || a.outcome === "cancelled" ? t(`mcp.outcome.${a.outcome}`) : t("mcp.outcome.failed")}</span>}
                </div>
                {out && openAct === key && (
                  <pre className="mcp-pop__out mono">{[out.stdout, out.stderr ? `${t("mcp.stderr")}\n${out.stderr}` : ""].filter(Boolean).join("\n")}</pre>
                )}
                </div>
                );
              })}
          </div>
        </div>,
        document.body,
      )}
    </>
  );
}
