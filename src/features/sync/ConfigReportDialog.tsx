import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { getSyncApi, type ConfigReport } from "./syncApi";
import "./sync.css";

/**
 * 关掉本机 Syncthing 管理页后，核对出的改动：
 * 加固项已经自动恢复（只说明一句）；任务设置的改动逐项“采用新值”或“恢复”；管理页里新加的只列出来。
 * 自己监听核对结果，有结果时弹出；页面重新加载后也会取回没处理完的。
 */
export function ConfigReportHost() {
  const { t } = useTranslation();
  const [report, setReport] = useState<ConfigReport | null>(null);
  const [busy, setBusy] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    let off: (() => void) | undefined;
    void getSyncApi().then(async (api) => {
      const pending = await api.configReport().catch(() => null);
      if (alive && pending) setReport(pending);
      const u = await api.onConfigReport((r) => { setError(null); setReport(r); });
      if (alive) off = u; else u();
    });
    return () => { alive = false; off?.(); };
  }, []);

  if (!report) return null;
  // 还有没处理的改动时只关掉对话框，后端保留着，下次进同步页再提示；否则清掉
  const close = () => { setReport(null); void getSyncApi().then((api) => api.configDismiss()).catch(() => {}); };
  const resolve = async (index: number, adopt: boolean) => {
    setBusy(index);
    setError(null);
    try {
      const rest = await (await getSyncApi()).configResolve(index, adopt);
      setReport(rest ?? (report.foreign.length || report.hardened.length ? { ...report, changes: [], pausedTask: null } : null));
    } catch (e) {
      setError(formatError(e));
    } finally {
      setBusy(null);
    }
  };
  const fieldName = (field: string) => t(`sync.config.field.${field}`, { defaultValue: field });

  return (
    <Dialog
      title={t("sync.config.title")}
      size="lg"
      onDismiss={busy === null ? close : undefined}
      actions={<Button variant="primary" disabled={busy !== null} onClick={close}>{report.changes.length ? t("sync.config.later") : t("mcp.gotIt")}</Button>}
    >
      <div className="cfg-report">
        {report.pausedTask && <p className="trash__status--danger">{t("sync.config.paused", { name: report.pausedTask })}</p>}
        {report.hardened.length > 0 && <p className="sync-muted">{t("sync.config.hardened", { count: report.hardened.length })}</p>}
        {report.changes.length > 0 && (
          <>
            <p className="wiz-hint">{t("sync.config.changesHint")}</p>
            <div className="cfg-report__list">
              {report.changes.map((c) => (
                <div key={c.index} className="cfg-report__row">
                  <div className="cfg-report__text">
                    <b className="pl-ellipsis">{c.taskName ?? c.id} · {fieldName(c.field)}</b>
                    <span className="mono pl-ellipsis" title={`${c.expected} → ${c.actual}`}>
                      {t("sync.config.values", { before: c.expected || "—", after: c.actual || "—" })}
                    </span>
                  </div>
                  <span className="cfg-report__actions">
                    {c.adoptable && <Button size="sm" disabled={busy !== null} onClick={() => void resolve(c.index, true)}>{t("sync.config.adopt")}</Button>}
                    <Button size="sm" disabled={busy !== null} onClick={() => void resolve(c.index, false)}>{t("sync.config.restore")}</Button>
                  </span>
                </div>
              ))}
            </div>
          </>
        )}
        {report.foreign.length > 0 && (
          <>
            <p className="wiz-hint">{t("sync.config.foreignHint")}</p>
            <ul className="cfg-report__foreign">
              {report.foreign.map((f) => (
                <li key={`${f.target}-${f.id}`} className="pl-ellipsis" title={f.path ?? f.id}>
                  {t(`sync.config.target.${f.target}`, { defaultValue: f.target })}：{f.name || f.id}{f.path ? ` · ${f.path}` : ""}
                </li>
              ))}
            </ul>
          </>
        )}
        <span className="trash__status trash__status--danger" role={error ? "alert" : undefined}>{error ?? ""}</span>
      </div>
    </Dialog>
  );
}
