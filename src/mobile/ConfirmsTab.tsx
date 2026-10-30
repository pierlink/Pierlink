import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type ConfirmItem, type Me } from "./api";
import { handleError } from "./App";

/** 待确认：AI 第一次连接、危险命令等，电脑上弹的同一个确认框，在这里也能回答 */
export function ConfirmsTab({ me, version, onAuthLost }: { me: Me; version: number; onAuthLost: () => void }) {
  const { t } = useTranslation();
  const [list, setList] = useState<ConfirmItem[] | null>(null);
  const [busy, setBusy] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    api.confirms().then((l) => { if (alive) { setList(l); setError(null); } }, (e: unknown) => { if (alive) setError(handleError(e, onAuthLost)); });
    return () => { alive = false; };
  }, [version, onAuthLost]);

  const answer = async (id: number, a: "allow" | "deny") => {
    setBusy(id);
    try {
      const ok = await api.answer(id, a);
      setList((l) => l?.filter((c) => c.id !== id) ?? null);
      setError(ok ? null : t("mobile.confirms.expired"));
    } catch (e) {
      setError(handleError(e, onAuthLost));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="m-page">
      {error && <p className="m-error">{error}</p>}
      {list === null && !error && <p className="m-muted">{t("common.loading")}</p>}
      {list?.length === 0 && <p className="m-muted">{t("mobile.confirms.none")}</p>}
      {!me.permissions.confirm && list && list.length > 0 && <p className="m-muted">{t("mobile.confirms.noPermission")}</p>}
      {list?.map((c) => (
        <section key={c.id} className={`m-card${c.kind === "dangerous" ? " m-card--danger" : ""}`}>
          <h2>{c.kind === "connect" ? t("mobile.confirms.connect") : c.kind === "dangerous" ? t("mobile.confirms.dangerous") : t("mobile.confirms.action")}</h2>
          <p className="m-muted">{c.client} · {c.entry === "windows" ? t("mcp.entryWindows") : c.entry}{c.target ? ` → ${c.target}` : ""}</p>
          {c.kind === "connect" && c.targets && c.targets.length > 0 && (
            <>
              <p className="m-muted">{t("mobile.confirms.targets")}</p>
              <ul className="m-list">{c.targets.map((x) => <li key={x}>{x}</li>)}</ul>
            </>
          )}
          {c.action && <pre className="m-code">{c.action}</pre>}
          {me.permissions.confirm && (
            <div className="m-actions">
              <button type="button" className="m-btn" disabled={busy !== null} onClick={() => void answer(c.id, "deny")}>{t("mcp.deny")}</button>
              <button type="button" className={`m-btn ${c.kind === "dangerous" ? "m-btn--danger" : "m-btn--primary"}`} disabled={busy !== null} onClick={() => void answer(c.id, "allow")}>{t("mcp.allowOnce")}</button>
            </div>
          )}
        </section>
      ))}
    </div>
  );
}
