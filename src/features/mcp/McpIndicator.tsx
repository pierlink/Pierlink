import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { Bot, CircleAlert } from "lucide-react";
import { Button } from "../../components";
import { getServersApi } from "../servers/api";
import { getMcpApi, type McpActivity, type McpNotice, type McpSession } from "./api";
import "./mcp.css";

/** 需要用户处理的提示（要登录、AI 终端在等密码） */
type Attention = { key: string; text: string };

/**
 * 标题栏上的 AI 接入状态：有 agent 连着或有需要处理的提示时出现。
 * 点开看连着的客户端、需要处理的提示、最近的操作；“全部停止”取消所有操作、断开所有 agent。
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
  // 最近两分钟有过操作：服务器入口的无握手客户端两次请求之间没有会话，也要让标题栏一直能看到
  const [recent, setRecent] = useState(false);
  const recentTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const btn = useRef<HTMLButtonElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  const namesRef = useRef(names);
  namesRef.current = names;

  const serverName = (id: string) => (id === "windows" ? t("mcp.targetWindows") : namesRef.current[id] ?? id);

  useEffect(() => {
    let alive = true;
    const offs: (() => void)[] = [];
    const keep = (u: () => void) => { if (alive) offs.push(u); else u(); };
    const refresh = async () => {
      const api = await getMcpApi();
      try {
        const s = await api.status();
        if (alive) setSessions(s.sessions);
      } catch { /* 后端还没准备好：下次事件再取 */ }
    };
    const noticeText = (n: McpNotice): Attention | null =>
      n.kind === "loginNeeded" ? { key: `login-${n.serverId}`, text: t("mcp.noticeLogin", { server: serverName(n.serverId) }) }
        : n.kind === "passwordPrompt" ? { key: `pw-${n.terminalId}`, text: t("mcp.noticePassword") }
          : null;
    void getMcpApi().then((api) => {
      void api.onSessions(() => void refresh()).then(keep);
      void api.onActivity((a) => {
        setActivity((list) => [a, ...list].slice(0, 30));
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
    return () => { alive = false; offs.forEach((u) => u()); if (recentTimer.current) clearTimeout(recentTimer.current); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!open) return;
    void getMcpApi().then((api) => api.activity(30)).then(setActivity, () => {});
    const onDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (!pop.current?.contains(target) && !btn.current?.contains(target)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") setOpen(false); };
    window.addEventListener("mousedown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("mousedown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open]);

  if (!sessions.length && !attention.length && !recent && !open) return null;

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

  return (
    <>
      <button
        ref={btn}
        type="button"
        className={`xfer-btn mcp-btn${sessions.length ? " xfer-btn--busy" : ""}`}
        aria-expanded={open}
        aria-haspopup="dialog"
        aria-label={t("mcp.indicator")}
        title={t("mcp.indicator")}
        onClick={() => { setOpen(!open); if (!open) setAttention([]); }}
      >
        <Bot size={14} />
        <span className="xfer-btn__text">{sessions.length ? t("mcp.connected", { count: sessions.length }) : t("mcp.indicatorShort")}</span>
        {attention.length > 0 && <span className="xfer-btn__dot" aria-hidden="true" />}
      </button>
      {open && r && createPortal(
        <div ref={pop} className="xfer-pop mcp-pop" role="dialog" aria-label={t("mcp.indicator")} style={{ top: r.bottom + 6, right: Math.max(8, window.innerWidth - r.right) }}>
          <div className="xfer-pop__head">
            <span className="xfer-pop__title">{t("mcp.indicator")}</span>
            {/* 最近有操作也给按钮：服务器入口的无握手客户端两次请求之间没有会话，被拒绝的决定也要靠它重置 */}
            {(sessions.length > 0 || recent) && <Button size="sm" variant="danger-ghost" disabled={stopping} onClick={() => void stopAll()}>{t("mcp.stopAll")}</Button>}
          </div>
          <div className="mcp-pop__body">
            {attention.map((a) => <div key={a.key} className="mcp-pop__attention"><CircleAlert size={13} />{a.text}</div>)}
            <div className="mcp-pop__section">{t("mcp.sessions")}</div>
            {sessions.length === 0
              ? <div className="xfer-pop__empty">{t(recent ? "mcp.noSessionsRecent" : "mcp.noSessions")}</div>
              : sessions.map((s) => <div key={s.id} className="mcp-pop__row pl-ellipsis">{s.client || t("mcp.unknownClient")} · {serverName(s.entry)}</div>)}
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
                  <span className="pl-ellipsis">{a.target ? `${serverName(a.target)}: ` : ""}{a.summary}</span>
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
