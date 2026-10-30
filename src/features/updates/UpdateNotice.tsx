import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Download } from "lucide-react";
import { Button, Dialog, Notice } from "../../components";
import { formatError } from "../../errors";
import { useSettings } from "../settings/store";
import { getUpdatesApi, type UpdateInfo, type UpdateProgress } from "./api";
import { pickNotes } from "./notes";
import "./updates.css";

/** 启动后多久检查一次更新（避开启动时的同步恢复、终端重连） */
const FIRST_CHECK_MS = 20_000;

/** 主动检查（设置页按钮）和启动后自动检查共用：把结果交给提示条 */
let showUpdate: ((u: UpdateInfo) => void) | null = null;
/** 设置页“检查更新”发现新版本后，弹出同一个提示条 */
export function announceUpdate(u: UpdateInfo) {
  showUpdate?.(u);
}

/**
 * 有新版本时的提示条（窗口底部）：显示版本和说明，“立即更新”后显示下载进度；
 * 下载完、签名校验通过后 Pierlink 先正常关闭（同步、终端、隧道），安装程序装好后自动重新打开。
 */
export function UpdateNotice() {
  const { t, i18n } = useTranslation();
  const { settings } = useSettings();
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [notesOpen, setNotesOpen] = useState(false);
  const [progress, setProgress] = useState<UpdateProgress | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    showUpdate = (u) => { setUpdate(u); setDismissed(false); };
    return () => { showUpdate = null; };
  }, []);

  useEffect(() => {
    if (!settings.updates.autoCheck) return;
    let alive = true;
    const timer = setTimeout(() => {
      void getUpdatesApi().then((api) => api.check()).then((u) => { if (alive && u) setUpdate(u); }, () => { /* 自动检查失败不打扰，设置页可以手动查 */ });
    }, FIRST_CHECK_MS);
    return () => { alive = false; clearTimeout(timer); };
  }, [settings.updates.autoCheck]);

  const install = () => {
    setError(null);
    setProgress({ kind: "downloading", downloaded: 0, total: null });
    getUpdatesApi()
      .then((api) => api.install(setProgress))
      .catch((e: unknown) => { setProgress(null); setError(formatError(e)); });
  };

  if (!update || dismissed) return null;
  const pct = progress?.kind === "downloading" && progress.total ? Math.min(100, Math.round((progress.downloaded / progress.total) * 100)) : null;
  return (
    <>
      <Notice order={2}>
      <div className="upd-banner" role="status">
        <Download size={16} aria-hidden />
        <span className="upd-banner__text">
          <b>{t("updates.available", { version: update.version })}</b>
          <span>{t("updates.availableHint", { current: update.currentVersion })}</span>
        </span>
        {update.notes && <Button size="sm" variant="ghost" onClick={() => setNotesOpen(true)}>{t("updates.notes")}</Button>}
        <Button size="sm" variant="primary" onClick={install}>{t("updates.install")}</Button>
        <Button size="sm" variant="ghost" onClick={() => setDismissed(true)}>{t("updates.later")}</Button>
      </div>
      </Notice>
      {notesOpen && (
        <Dialog title={t("updates.notesTitle", { version: update.version })} onDismiss={() => setNotesOpen(false)} actions={<Button onClick={() => setNotesOpen(false)}>{t("common.close")}</Button>}>
          <pre className="upd-notes">{pickNotes(update.notes ?? "", i18n.language)}</pre>
        </Dialog>
      )}
      {(progress || error) && (
        <Dialog
          title={t("updates.installing", { version: update.version })}
          onDismiss={error ? () => setError(null) : undefined}
          actions={error ? <Button onClick={() => setError(null)}>{t("common.close")}</Button> : null}
        >
          {error ? (
            <p className="upd-error">{t("updates.failed", { text: error })}</p>
          ) : (
            <>
              <p>{progress?.kind === "installing" ? t("updates.restarting") : t("updates.downloading")}</p>
              <div className="upd-progress" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct ?? undefined}>
                <div className="upd-progress__bar" style={{ width: `${pct ?? 100}%` }} data-indeterminate={pct === null ? "" : undefined} />
              </div>
            </>
          )}
        </Dialog>
      )}
    </>
  );
}
