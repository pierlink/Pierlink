import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { normalizeSettings } from "./model";
import { useSettings } from "./store";
import type { ImportResult } from "./profileApi";

/**
 * 导入结果：新增多少、跳过了什么、哪些私钥在这台电脑上找不到；
 * 完整备份还能选择套用文件里的应用设置。serversOnly 时只说服务器。
 */
export function ImportReportDialog({ result, serversOnly = false, onClose }: { result: ImportResult; serversOnly?: boolean; onClose: () => void }) {
  const { t } = useTranslation();
  const { update } = useSettings();
  const [applied, setApplied] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const apply = () => {
    if (!result.appSettings) return;
    try {
      update(() => normalizeSettings(JSON.parse(result.appSettings!)));
      setApplied(true);
    } catch (e) {
      setError(formatError(e));
    }
  };

  return (
    <Dialog title={t("backup.reportTitle")} onDismiss={onClose} actions={<Button variant="primary" onClick={onClose}>{t("common.close")}</Button>}>
      <div className="backup__result">
        <p>{serversOnly ? t("backup.importedServers", { count: result.serversAdded }) : t("backup.imported", { servers: result.serversAdded, forwards: result.forwardsAdded, snippets: result.snippetsAdded })}</p>
        {result.mcpEntriesAdded > 0 && <p className="set-row__hint">{t("backup.mcpAdded", { count: result.mcpEntriesAdded })}</p>}
        {result.serversAdded > 0 && <p className="set-row__hint">{t("backup.fingerprintNote")}</p>}
        {result.notes.length > 0 && (
          <ul className="backup__notes">
            {result.notes.map((n, i) => (
              <li key={`${n.name}-${i}`} title={n.path ?? undefined}>
                <b>{n.name}</b>{t("backup.noteSep")}{t(`backup.reason.${n.reason}`, { defaultValue: n.reason, path: n.path ?? "" })}
              </li>
            ))}
          </ul>
        )}
        {!serversOnly && result.appSettings && (
          <div className="backup__apply">
            <span className="set-row__hint">{t("backup.appSettingsHint")}</span>
            <Button size="sm" disabled={applied} onClick={apply}>{applied ? t("backup.applied") : t("backup.apply")}</Button>
          </div>
        )}
        {error && <p className="backup__status backup__status--error">{error}</p>}
      </div>
    </Dialog>
  );
}
