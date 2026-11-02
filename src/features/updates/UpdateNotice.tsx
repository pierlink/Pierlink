import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Download } from "lucide-react";
import { Button, Dialog, Notice } from "../../components";
import { formatError, isAppError } from "../../errors";
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
 * 有新版本时的提示条（窗口底部）：显示版本和说明。“下载更新”只下载并核对签名，下完由用户点“安装更新”才装：
 * Pierlink 先正常关闭（同步、终端、隧道），安装程序装好后自动重新打开。选“稍后”时提示条上的按钮变成“安装更新”。
 */
export function UpdateNotice() {
  const { t, i18n } = useTranslation();
  const { settings } = useSettings();
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [notesOpen, setNotesOpen] = useState(false);
  const [progress, setProgress] = useState<UpdateProgress | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** 下载好的版本号（还没下或下的不是这个版本时为 null） */
  const [ready, setReady] = useState<string | null>(null);
  /** 下载好后弹出的“安装更新”框开着 */
  const [askInstall, setAskInstall] = useState(false);
  /** 点了“安装更新”：Pierlink 马上关闭 */
  const [installing, setInstalling] = useState(false);

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

  // 第几次点“下载更新”：只认最新这次的进度和结果（后端同一时间只下载一个，换掉的那个返回 update.superseded）
  const attempt = useRef(0);
  const download = () => {
    const mine = ++attempt.current;
    setError(null);
    setProgress({ kind: "downloading", downloaded: 0, total: null });
    getUpdatesApi()
      .then((api) => api.download((p) => { if (attempt.current === mine) setProgress(p); }))
      .then((version) => {
        if (attempt.current !== mine) return;
        setProgress(null);
        setReady(version);
        setAskInstall(true);
      })
      .catch((e: unknown) => {
        if (attempt.current !== mine || (isAppError(e) && e.code === "update.superseded")) return;
        setProgress(null);
        setError(formatError(e));
      });
  };
  const install = () => {
    setError(null);
    setInstalling(true);
    getUpdatesApi()
      .then((api) => api.install())
      .catch((e: unknown) => {
        setInstalling(false);
        setAskInstall(false);
        setError(formatError(e));
        // 下载好的不是最新版本（后来又出了新版本，旧包已删）或者包不见了：重新查一次，按钮变回“下载更新”
        if (isAppError(e) && (e.code === "update.outdated" || e.code === "update.not_downloaded")) {
          setReady(null);
          void getUpdatesApi().then((api) => api.check()).then((u) => { if (u) setUpdate(u); }, () => {});
        }
      });
  };

  if (!update || dismissed) return null;
  // 下载好的就是提示条上的这个版本（这次下的，或者检查时后端说之前已经下好了）
  const downloaded = ready === update.version || (update.downloaded && ready === null);
  const pct = progress?.total ? Math.min(100, Math.round((progress.downloaded / progress.total) * 100)) : null;
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
        {downloaded
          ? <Button size="sm" variant="primary" disabled={installing} onClick={install}>{t("updates.installNow")}</Button>
          : <Button size="sm" variant="primary" disabled={progress !== null} onClick={download}>{t("updates.download")}</Button>}
        <Button size="sm" variant="ghost" onClick={() => setDismissed(true)}>{t("updates.later")}</Button>
      </div>
      </Notice>
      {notesOpen && (
        <Dialog title={t("updates.notesTitle", { version: update.version })} onDismiss={() => setNotesOpen(false)} actions={<Button onClick={() => setNotesOpen(false)}>{t("common.close")}</Button>}>
          <pre className="upd-notes">{pickNotes(update.notes ?? "", i18n.language)}</pre>
        </Dialog>
      )}
      {(progress || error || installing || (askInstall && downloaded)) && (
        <Dialog
          title={installing || (askInstall && downloaded && !error) ? t("updates.installTitle", { version: update.version }) : t("updates.installing", { version: update.version })}
          onDismiss={error ? () => setError(null) : askInstall && !installing ? () => setAskInstall(false) : undefined}
          actions={error
            ? <><Button onClick={() => setError(null)}>{t("common.close")}</Button><Button variant="primary" onClick={downloaded ? install : download}>{t("common.retry")}</Button></>
            : askInstall && downloaded && !installing
              ? <><Button onClick={() => setAskInstall(false)}>{t("updates.later")}</Button><Button variant="primary" onClick={install}>{t("updates.installNow")}</Button></>
              : null}
        >
          {error ? (
            <p className="upd-error">{t("updates.failed", { text: error })}</p>
          ) : installing ? (
            <p>{t("updates.restarting")}</p>
          ) : askInstall && downloaded ? (
            <p>{t("updates.readyHint")}</p>
          ) : (
            <>
              <p>{t("updates.downloading")}</p>
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
