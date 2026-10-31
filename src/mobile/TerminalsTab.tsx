import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, ApiError, type Me, type Screen, type TerminalItem } from "./api";
import { handleError } from "./App";

/** 键盘栏：手机键盘上没有的键（名字和手机入口接受的一致） */
const KEYS: { key: string; label: string }[] = [
  { key: "Ctrl+C", label: "Ctrl+C" }, { key: "Tab", label: "Tab" }, { key: "Escape", label: "Esc" },
  { key: "ArrowUp", label: "↑" }, { key: "ArrowDown", label: "↓" }, { key: "Enter", label: "⏎" },
];

/** 终端：电脑上开着的终端列表，点开看屏幕；有输入权限时可以往里打字 */
export function TerminalsTab({ me, version, onAuthLost }: { me: Me; version: number; onAuthLost: () => void }) {
  const { t } = useTranslation();
  const [list, setList] = useState<TerminalItem[] | null>(null);
  const [open, setOpen] = useState<TerminalItem | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    api.terminals().then((l) => {
      if (!alive) return;
      setList(l);
      setError(null);
      // 打开着的终端被关掉了：回到列表
      setOpen((o) => (o && !l.some((x) => x.id === o.id) ? null : o));
    }, (e: unknown) => { if (alive) setError(handleError(e, onAuthLost)); });
    return () => { alive = false; };
  }, [version, onAuthLost]);

  if (open) return <TerminalView me={me} term={open} onBack={() => setOpen(null)} onAuthLost={onAuthLost} />;
  return (
    <div className="m-page">
      {error && <p className="m-error">{error}</p>}
      {list === null && !error && <p className="m-muted">{t("common.loading")}</p>}
      {list?.length === 0 && <p className="m-muted">{t("mobile.terminals.none")}</p>}
      {list?.map((x) => (
        <button key={x.id} type="button" className="m-row m-row--button" onClick={() => setOpen(x)}>
          <span className="m-row__main">{x.title}</span>
          <span className="m-row__side">
            {x.ai ? "AI · " : ""}{x.kind === "windows" ? t("mobile.terminals.local") : x.server ?? ""}{x.busy ? ` · ${t("mobile.terminals.busy")}` : ""}
          </span>
        </button>
      ))}
    </div>
  );
}

function TerminalView({ me, term, onBack, onAuthLost }: { me: Me; term: TerminalItem; onBack: () => void; onAuthLost: () => void }) {
  const { t } = useTranslation();
  const [screen, setScreen] = useState<Screen | null>(null);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [danger, setDanger] = useState<{ text: string | null; keys: string[]; lines: string[] } | null>(null);
  const pre = useRef<HTMLPreElement>(null);

  // 看着的时候每秒拉一次屏幕
  useEffect(() => {
    let alive = true;
    const load = () => api.screen(term.id).then((s) => { if (alive) setScreen(s); }, (e: unknown) => { if (alive) setStatus(handleError(e, onAuthLost)); });
    void load();
    const timer = setInterval(() => void load(), 1000);
    return () => { alive = false; clearInterval(timer); };
  }, [term.id, onAuthLost]);
  useEffect(() => { pre.current?.scrollTo({ top: pre.current.scrollHeight }); }, [screen?.text]);

  const send = async (txt: string | null, keys: string[], confirmed = false) => {
    setBusy(true);
    setStatus(null);
    try {
      await api.input(term.id, txt, keys, confirmed);
      if (txt) setText("");
      setDanger(null);
    } catch (e) {
      if (e instanceof ApiError && e.status === 409 && e.body?.needs_confirm) {
        setDanger({ text: txt, keys, lines: Array.isArray(e.body.lines) ? (e.body.lines as string[]) : [] });
      } else {
        setStatus(handleError(e, onAuthLost));
      }
    } finally {
      setBusy(false);
    }
  };
  const canType = me.permissions.input && !screen?.password;

  return (
    <div className="m-term">
      <div className="m-term__bar">
        <button type="button" className="m-btn m-btn--small" onClick={onBack}>{t("mobile.back")}</button>
        <span className="m-term__title">{term.title}</span>
      </div>
      <pre ref={pre} className="m-term__screen">{screen?.text ?? t("common.loading")}</pre>
      {screen?.password && <p className="m-warn">{t("mobile.terminals.password")}</p>}
      {danger && (
        <div className="m-sheet" role="alertdialog">
          <p><b>{t("mobile.terminals.dangerTitle")}</b></p>
          <pre className="m-code">{danger.lines.join("\n")}</pre>
          <div className="m-actions">
            <button type="button" className="m-btn" onClick={() => setDanger(null)}>{t("common.cancel")}</button>
            <button type="button" className="m-btn m-btn--danger" disabled={busy} onClick={() => void send(danger.text, danger.keys, true)}>{t("mobile.terminals.runAnyway")}</button>
          </div>
        </div>
      )}
      {me.permissions.input ? (
        <div className="m-term__input">
          <div className="m-keys">
            {KEYS.map((k) => <button key={k.key} type="button" className="m-key" disabled={busy || !me.permissions.input} onClick={() => void send(null, [k.key])}>{k.label}</button>)}
          </div>
          {/* 文字和按键不能一起发：回车直接放在文字末尾 */}
          <form className="m-term__line" onSubmit={(e) => { e.preventDefault(); if (text) void send(`${text}\r`, []); }}>
            <input value={text} disabled={!canType} autoCapitalize="off" autoCorrect="off" spellCheck={false} placeholder={canType ? t("mobile.terminals.placeholder") : ""} onChange={(e) => setText(e.target.value)} />
            <button type="submit" className="m-btn m-btn--primary" disabled={busy || !canType || !text}>{t("mobile.terminals.send")}</button>
          </form>
        </div>
      ) : (
        <p className="m-muted">{t("mobile.terminals.noInput")}</p>
      )}
      <p className="m-status" role="status">{status ?? ""}</p>
    </div>
  );
}
