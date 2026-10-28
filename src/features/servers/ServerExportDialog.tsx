import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Button, Checkbox, Dialog } from "../../components";
import { formatError } from "../../errors";
import { exportServers } from "../settings/profileApi";
import type { ServerRecord } from "./api";
import "./server-export.css";

/** 导出服务器：勾选要给别人的服务器，存成文件；对方在“导入服务器 → 从文件导入”里导入 */
export function ServerExportDialog({ servers: all, initial, onClose }: { servers: ServerRecord[]; initial: string | null; onClose: () => void }) {
  const { t } = useTranslation();
  // 从 ssh config 导入的服务器跟着本机的配置文件走，给别人没用
  const servers = all.filter((s) => s.source !== "ssh_config");
  const [picked, setPicked] = useState<Set<string>>(() => new Set(initial ? [initial] : servers.map((s) => s.id)));
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<{ ok: boolean; text: string } | null>(null);
  const allPicked = picked.size === servers.length;

  const toggle = (id: string, on: boolean) => setPicked((p) => {
    const next = new Set(p);
    if (on) next.add(id); else next.delete(id);
    return next;
  });
  const run = async () => {
    setBusy(true);
    setStatus(null);
    try {
      const path = await exportServers(servers.filter((s) => picked.has(s.id)).map((s) => s.id));
      if (path) setStatus({ ok: true, text: t("serverExport.done", { path }) });
    } catch (e) {
      setStatus({ ok: false, text: formatError(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog title={t("serverExport.title")} onDismiss={busy ? undefined : onClose} actions={<>
      <Button onClick={onClose} disabled={busy}>{status?.ok ? t("common.close") : t("common.cancel")}</Button>
      <Button variant="primary" disabled={busy || picked.size === 0} onClick={() => void run()}>{t("serverExport.button", { count: picked.size })}</Button>
    </>}>
      <p className="set-row__hint">{t("serverExport.hint")}</p>
      <div className="srv-export__all">
        <Checkbox label={t("serverExport.all")} checked={allPicked} onChange={(e) => setPicked(e.target.checked ? new Set(servers.map((s) => s.id)) : new Set())} />
      </div>
      <div className="srv-export__list">
        {servers.map((s) => (
          <label key={s.id} className="srv-export__row">
            <input type="checkbox" checked={picked.has(s.id)} onChange={(e) => toggle(s.id, e.target.checked)} />
            <span className="pl-ellipsis">{s.name}</span>
            <span className="srv-export__addr pl-ellipsis mono">{s.username ? `${s.username}@` : ""}{s.host ?? s.sshAlias ?? ""}</span>
          </label>
        ))}
      </div>
      <p className={`srv-export__status pl-ellipsis${status ? (status.ok ? " srv-export__status--ok" : " srv-export__status--error") : ""}`} role="status" title={status?.text}>
        {busy ? t("backup.exporting") : status?.text ?? ""}
      </p>
    </Dialog>
  );
}
