import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { Download } from "lucide-react";
import { Button, Dialog, useAnyDialogOpen, useBottomOccupied } from "../../components";
import { formatError, isAppError } from "../../errors";
import { useSettings } from "../settings/store";
import { getUpdatesApi, type UpdateInfo, type UpdateProgress } from "./api";
import { pickNotes } from "./notes";
import { clearSnooze, snooze, snoozed } from "./snooze";
import "./updates.css";

/** 启动后多久检查一次更新（避开启动时的同步恢复、终端重连） */
const FIRST_CHECK_MS = 20_000;
/** 之后每隔多久再查一次：常驻托盘、几天不重启的人也能收到新版本 */
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;
/** 自动检查失败后多久再试：开机自动启动时第一次检查可能赶在联网之前；一直连不上时越隔越久，最长和定时检查一样 */
const RETRY_MS = [10, 30, 60, 180, 360].map((m) => m * 60 * 1000);

/** 主动检查（设置页按钮）和启动后自动检查共用：把结果交给提示条 */
let showUpdate: ((u: UpdateInfo) => void) | null = null;
/** 设置页“检查更新”发现新版本后，弹出同一个提示条 */
export function announceUpdate(u: UpdateInfo) {
  showUpdate?.(u);
}

/**
 * 有新版本时的提示卡片（窗口右下角）：显示版本和说明。启动 20 秒后查一次，之后每 6 小时查一次；
 * 点“稍后”时这个版本一天内不再提示（过了一天还没更新再提示一次，出了更新的版本马上提示）。“下载更新”只下载并核对签名，下完由用户点“安装更新”才装：
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
  // 有对话框开着（包括这里的更新说明）时提示条先不显示：它浮在最上层，会压住对话框
  const dialogOpen = useAnyDialogOpen();
  // 窗口底部有提示条、删除浮窗或文本编辑器时也先不显示，免得盖住它们的按钮
  const bottomBusy = useBottomOccupied();
  /** 正在下载、安装或等用户点安装：这时定时检查说没有新版本也不撤卡片 */
  const busy = useRef(false);
  useEffect(() => { busy.current = progress !== null || installing || askInstall; });
  useEffect(() => {
    // 自己点了“检查更新”：之前的“稍后”不算了
    showUpdate = (u) => { clearSnooze(); setUpdate(u); setDismissed(false); };
    return () => { showUpdate = null; };
  }, []);

  useEffect(() => {
    if (!settings.updates.autoCheck) return;
    let alive = true;
    let retry: ReturnType<typeof setTimeout> | undefined;
    /** 连续失败了几次 */
    let fails = 0;
    const check = () => {
      clearTimeout(retry);
      void getUpdatesApi().then((api) => api.check()).then((u) => {
        if (!alive) return;
        fails = 0;
        // 服务器上已经没有新版本（比如那个版本撤了）：撤掉卡片
        if (!u) {
          if (!busy.current) setUpdate(null);
          return;
        }
        setUpdate(u);
        setDismissed(snoozed(u.version));
      }, () => {
        // 自动检查失败不打扰，过一会儿再查；设置页也可以手动查
        if (alive) retry = setTimeout(check, RETRY_MS[Math.min(fails++, RETRY_MS.length - 1)]);
      });
    };
    const first = setTimeout(check, FIRST_CHECK_MS);
    const every = setInterval(check, CHECK_EVERY_MS);
    return () => { alive = false; clearTimeout(first); clearTimeout(retry); clearInterval(every); };
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

  const floatRef = useRef<HTMLDivElement | null>(null);
  const cardShown = !!update && !dismissed && !dialogOpen && !bottomBusy;
  // 卡片显示时把它的高度告诉页面（--upd-float-h）：文件列表等滚动区底部留出这么高，最后几行能滚到卡片上面。
  // 卡片收起后也不清：清了会让滚到底的列表跳回去，所以这次运行里出过卡片就一直留着这块空
  useLayoutEffect(() => {
    const el = floatRef.current;
    if (!cardShown || !el) return;
    const root = document.documentElement;
    const apply = () => {
      const h = Math.ceil(el.getBoundingClientRect().height);
      if (h > 0) root.style.setProperty("--upd-float-h", `${h}px`);
    };
    apply();
    const ro = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(apply);
    ro?.observe(el);
    return () => ro?.disconnect();
  }, [cardShown]);

  if (!update || dismissed) return null;
  // 下载好的就是提示条上的这个版本（这次下的，或者检查时后端说之前已经下好了）
  const downloaded = ready === update.version || (update.downloaded && ready === null);
  const pct = progress?.total ? Math.min(100, Math.round((progress.downloaded / progress.total) * 100)) : null;
  const action = downloaded
    ? <Button size="sm" variant="primary" disabled={installing} onClick={() => { setNotesOpen(false); install(); }}>{t("updates.installNow")}</Button>
    : <Button size="sm" variant="primary" disabled={progress !== null} onClick={() => { setNotesOpen(false); download(); }}>{t("updates.download")}</Button>;
  const later = () => {
    snooze(update.version);
    setDismissed(true);
  };
  return (
    <>
      {cardShown && createPortal(
        <div className="upd-float" ref={floatRef}>
          <div className="upd-banner" role="status">
            <div className="upd-banner__head">
              <Download size={16} aria-hidden />
              <span className="upd-banner__text">
                <b>{t("updates.available", { version: update.version })}</b>
                <span>{t("updates.availableHint", { current: update.currentVersion })}</span>
              </span>
            </div>
            <div className="upd-banner__actions">
              {update.notes && <Button size="sm" variant="ghost" onClick={() => setNotesOpen(true)}>{t("updates.notes")}</Button>}
              <Button size="sm" variant="ghost" onClick={later}>{t("updates.later")}</Button>
              {action}
            </div>
          </div>
        </div>,
        document.body,
      )}
      {notesOpen && (
        <Dialog size="lg" title={t("updates.notesTitle", { version: update.version })} onDismiss={() => setNotesOpen(false)}
          actions={<><Button onClick={() => setNotesOpen(false)}>{t("common.close")}</Button>{action}</>}>
          {/* 能选中、复制；打开时焦点在这里，方向键、PageDown 可以翻 */}
          <pre className="upd-notes" tabIndex={0}>{pickNotes(update.notes ?? "", i18n.language)}</pre>
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
