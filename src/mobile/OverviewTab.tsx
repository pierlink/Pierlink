import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type Me, type Overview } from "./api";
import { handleError } from "./App";

/** 概况：服务器在线和资源、同步任务、AI 连接；有确认权限时可以全部停止 AI；最下面可以在这台手机上退出 */
export function OverviewTab({ me, version, onAuthLost, onSignedOut }: { me: Me; version: number; onAuthLost: () => void; onSignedOut: () => void }) {
  const { t } = useTranslation();
  const [data, setData] = useState<Overview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [stopping, setStopping] = useState<"idle" | "ask" | "busy" | "done">("idle");
  const [leaving, setLeaving] = useState<"idle" | "ask" | "busy">("idle");

  useEffect(() => {
    let alive = true;
    const load = () => api.overview().then((d) => { if (alive) { setData(d); setError(null); } }, (e: unknown) => { if (alive) setError(handleError(e, onAuthLost)); });
    void load();
    // 资源数字没有事件，看着的时候每 10 秒拉一次（页面在后台时不拉）
    const timer = setInterval(() => { if (document.visibilityState === "visible") void load(); }, 10_000);
    return () => { alive = false; clearInterval(timer); };
  }, [version, onAuthLost]);

  const signOut = async () => {
    setLeaving("busy");
    try { await api.logout(); onSignedOut(); } catch (e) { setError(handleError(e, onAuthLost)); setLeaving("idle"); }
  };
  const stopAll = async () => {
    setStopping("busy");
    try { await api.stopAll(); setStopping("done"); } catch (e) { setError(handleError(e, onAuthLost)); setStopping("idle"); }
  };

  return (
    <div className="m-page">
      {error && <p className="m-error">{error}</p>}
      {!data && !error && <p className="m-muted">{t("common.loading")}</p>}
      {data && (
        <>
          <section className="m-card">
            <h2>{t("mobile.overview.servers")}</h2>
            {data.servers.length === 0 && <p className="m-muted">{t("mobile.overview.noServers")}</p>}
            {data.servers.map((s) => (
              <div key={s.id} className="m-row">
                <span className={`m-dot${s.online ? " m-dot--on" : ""}`} aria-hidden="true" />
                <span className="m-row__main">{s.name}</span>
                <span className="m-row__side">
                  {s.cpu !== null ? `CPU ${Math.round(s.cpu)}%` : ""}
                  {s.mem !== null ? ` · ${t("monitor.memoryShort")} ${Math.round(s.mem)}%` : ""}
                  {s.online === false ? t("mobile.overview.offline") : ""}
                </span>
              </div>
            ))}
          </section>
          <section className="m-card">
            <h2>{t("mobile.overview.sync")}</h2>
            {data.sync.length === 0 && <p className="m-muted">{t("mobile.overview.noSync")}</p>}
            {data.sync.map((s) => (
              <div key={s.id} className="m-row">
                <span className="m-row__main">{s.name}</span>
                <span className="m-row__side">
                  {s.state === "syncing" && s.percent === null ? t("mobile.overview.syncing") : t(`sync.state.${s.state}`, { percent: s.percent ?? 0, count: 0, defaultValue: s.state })}
                </span>
              </div>
            ))}
          </section>
          <section className="m-card">
            <h2>{t("mobile.overview.ai")}</h2>
            <p>{t("mobile.overview.aiState", { sessions: data.ai.sessions, pending: data.ai.pending })}</p>
            {me.permissions.confirm && (
              stopping === "ask" ? (
                <div className="m-actions">
                  <button type="button" className="m-btn m-btn--danger" onClick={() => void stopAll()}>{t("mobile.overview.stopConfirm")}</button>
                  <button type="button" className="m-btn" onClick={() => setStopping("idle")}>{t("common.cancel")}</button>
                </div>
              ) : (
                // 没有连着的 AI 也能点：AI 开的后台任务可能还在跑
                <button type="button" className="m-btn m-btn--danger" disabled={stopping === "busy"} onClick={() => setStopping("ask")}>
                  {stopping === "done" ? t("mobile.overview.stopped") : t("mcp.stopAll")}
                </button>
              )
            )}
          </section>
        </>
      )}
      <section className="m-signout">
        {leaving === "ask" ? (
          <div className="m-actions">
            <button type="button" className="m-btn m-btn--danger" onClick={() => void signOut()}>{t("mobile.signOutConfirm")}</button>
            <button type="button" className="m-btn" onClick={() => setLeaving("idle")}>{t("common.cancel")}</button>
          </div>
        ) : (
          <button type="button" className="m-btn m-btn--plain" disabled={leaving === "busy"} onClick={() => setLeaving("ask")}>{t("mobile.signOut")}</button>
        )}
      </section>
    </div>
  );
}
