import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Download, Upload } from "lucide-react";
import { Button } from "../../components";
import { formatError } from "../../errors";
import { notifyServersChanged } from "../servers/api";
import { useSettings } from "./store";
import { exportProfile, importProfile, type ImportResult } from "./profileApi";
import { ImportReportDialog } from "./ImportReportDialog";

/**
 * 设置页“备份和迁移”：把服务器、端口转发、片段、应用设置和 MCP 设置导出到一个文件，
 * 在另一台电脑上导入。不含密码、令牌和同步任务。结果写在固定位置，不挤动下面的内容。
 */
export function BackupSection() {
  const { t } = useTranslation();
  const { settings } = useSettings();
  const [busy, setBusy] = useState<"export" | "import" | null>(null);
  const [status, setStatus] = useState<{ ok: boolean; text: string } | null>(null);
  const [result, setResult] = useState<ImportResult | null>(null);

  const doExport = async () => {
    setBusy("export");
    setStatus(null);
    try {
      const path = await exportProfile(JSON.stringify(settings));
      if (path) setStatus({ ok: true, text: t("backup.exported", { path }) });
    } catch (e) {
      setStatus({ ok: false, text: formatError(e) });
    } finally {
      setBusy(null);
    }
  };
  const doImport = async () => {
    setBusy("import");
    setStatus(null);
    try {
      const r = await importProfile();
      if (!r) return;
      setResult(r);
      notifyServersChanged();
      setStatus({ ok: true, text: t("backup.imported", { servers: r.serversAdded, forwards: r.forwardsAdded, snippets: r.snippetsAdded }) });
    } catch (e) {
      setStatus({ ok: false, text: formatError(e) });
    } finally {
      setBusy(null);
    }
  };
  return (
    <div className="set-card backup">
      <div className="set-row">
        <span className="set-row__text">
          <span>{t("backup.export")}</span>
          <span className="set-row__hint">{t("backup.exportHint")}</span>
        </span>
        <span className="set-row__control">
          <Button size="sm" icon={<Download size={13} />} disabled={busy !== null} onClick={() => void doExport()}>{t("backup.exportButton")}</Button>
        </span>
      </div>
      <div className="set-row">
        <span className="set-row__text">
          <span>{t("backup.import")}</span>
          <span className="set-row__hint">{t("backup.importHint")}</span>
        </span>
        <span className="set-row__control">
          <Button size="sm" icon={<Upload size={13} />} disabled={busy !== null} onClick={() => void doImport()}>{t("backup.importButton")}</Button>
        </span>
      </div>
      <p className={`backup__status pl-ellipsis${status ? (status.ok ? " backup__status--ok" : " backup__status--error") : ""}`} role="status" title={status?.text}>
        {busy ? t(busy === "export" ? "backup.exporting" : "backup.importing") : status?.text ?? ""}
      </p>
      {result && <ImportReportDialog result={result} onClose={() => setResult(null)} />}
    </div>
  );
}
