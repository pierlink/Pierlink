import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Checkbox, Dialog, TextField } from "../../components";
import { formatBytes } from "./format";
import type { UninstallChoice, UninstallReportView } from "./types";
import "./sync.css";

interface Props {
  report: UninstallReportView;
  busy: boolean;
  onConfirm: (choice: UninstallChoice) => void;
  onClose: () => void;
}

/**
 * 卸载服务器端同步组件：只删除安装清单里的东西，列清“将删除 / 不会删除”，
 * 输入服务器名后才能确认。旧版本与冲突存档默认保留，避免丢失用户内容。
 */
export function UninstallDialog({ report, busy, onConfirm, onClose }: Props) {
  const { t } = useTranslation();
  const [typed, setTyped] = useState("");
  const [choice, setChoice] = useState<UninstallChoice>({ keepVersions: true, keepConfig: false, removeKey: false });
  const archiveBytes = report.versionsBytes + report.conflictsBytes;
  const confirmed = typed.trim() === report.serverName;

  return (
    <Dialog
      title={t("uninstall.title", { server: report.serverName })}
      size="lg"
      danger
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="danger" disabled={!confirmed || busy} onClick={() => onConfirm(choice)}>
          {busy ? t("uninstall.removing") : t("uninstall.confirm")}
        </Button>
      </>}
    >
      <div className="uninst">
        <p>{t("uninstall.lead", { count: report.tasks.length })}</p>

        {report.otherComputers.length > 0 && (
          <Banner tone="danger" title={t("uninstall.othersTitle", { count: report.otherComputers.length })}>
            {t("uninstall.othersBody", { list: report.otherComputers.join(t("common.listSep")) })}
          </Banner>
        )}
        {report.ownSyncthing && (
          <Banner tone="ok" title={t("uninstall.ownTitle")}>
            {t("uninstall.ownBody")}
            <code className="mono sync-cmd">{report.ownSyncthing.binary} · {report.ownSyncthing.configDir}</code>
          </Banner>
        )}

        <section className="uninst__group">
          <span className="uninst__label uninst__label--remove">{t("uninstall.willRemove")}</span>
          {report.remove.map((r) => (
            <div key={r.path} className="uninst__row">
              <span className="mono" title={r.path}>{r.path}</span>
              <span className="sync-muted">{t(`uninstall.kind.${r.kind}`)}{r.bytes !== null ? ` · ${formatBytes(r.bytes)}` : ""}</span>
            </div>
          ))}
          {!choice.keepVersions && archiveBytes > 0 && (
            <div className="uninst__row uninst__row--danger">
              <span>{t("uninstall.archiveRow")}</span>
              <span>{formatBytes(archiveBytes)}</span>
            </div>
          )}
        </section>

        <section className="uninst__group">
          <span className="uninst__label uninst__label--keep">{t("uninstall.willKeep")}</span>
          <ul className="uninst__keep">
            {report.tasks.length > 0 && <li>{t("uninstall.keepRemote")}<span className="mono">{report.tasks.map((x) => x.remotePath).join(t("common.listSep"))}</span></li>}
            {report.tasks.length > 0 && <li>{t("uninstall.keepLocal")}<span className="mono">{report.tasks.map((x) => x.localPath).join(t("common.listSep"))}</span></li>}
            {choice.keepVersions && archiveBytes > 0 && <li>{t("uninstall.keepArchive", { size: formatBytes(archiveBytes) })}</li>}
            <li>{t("uninstall.keepOther")}</li>
          </ul>
        </section>

        <div className="uninst__options">
          {archiveBytes > 0 && (
            <Checkbox
              label={t("uninstall.optVersions", { size: formatBytes(archiveBytes) })}
              checked={choice.keepVersions}
              onChange={(e) => setChoice({ ...choice, keepVersions: e.target.checked })}
            />
          )}
          <Checkbox label={t("uninstall.optConfig")} checked={choice.keepConfig} onChange={(e) => setChoice({ ...choice, keepConfig: e.target.checked })} />
          {report.hasDedicatedKey && (
            <Checkbox label={t("uninstall.optKey")} checked={choice.removeKey} onChange={(e) => setChoice({ ...choice, removeKey: e.target.checked })} />
          )}
        </div>
        {!choice.keepVersions && archiveBytes > 0 && <Banner tone="warn" title={t("uninstall.archiveWarnTitle")}>{t("uninstall.archiveWarnBody")}</Banner>}

        <TextField
          label={t("uninstall.typeName", { server: report.serverName })}
          className="pl-input mono"
          value={typed}
          autoComplete="off"
          spellCheck={false}
          onChange={(e) => setTyped(e.target.value)}
        />
      </div>
    </Dialog>
  );
}
