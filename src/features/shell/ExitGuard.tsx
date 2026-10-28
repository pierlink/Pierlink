import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { formatBytes } from "../sync/format";
import { useSettings } from "../settings/store";
import { inTauri } from "./TitleBar";
import "./shell.css";

/** 与 Rust 端 `app_shell::EXIT_EVENT` 一致 */
const EXIT_EVENT = "pierlink://exit-requested";

export interface ExitSummary {
  terminals: number;
  tasks: { name: string; needFiles: number; needBytes: number }[];
  syncUnknown: boolean;
}

type ShellCommands = (typeof import("../../bindings/commands"))["commands"];
let commandsPromise: Promise<ShellCommands> | null = null;
function shell(): Promise<ShellCommands> {
  commandsPromise ??= import("../../bindings/commands").then((m) => m.commands);
  return commandsPromise;
}

/** 托盘是否可用（只查一次；查询失败按不可用处理，关闭窗口会走退出确认） */
let trayPromise: Promise<boolean> | null = null;
export function trayAvailable(): Promise<boolean> {
  if (!inTauri) return Promise.resolve(false);
  trayPromise ??= shell().then((c) => c.appTrayAvailable()).catch(() => false);
  return trayPromise;
}

/** 摘要里没有任何会丢失或中断的内容时，直接退出不打扰用户 */
function nothingToLose(s: ExitSummary): boolean {
  return s.terminals === 0 && s.tasks.length === 0 && !s.syncUnknown;
}

/**
 * 退出确认（设计 1.4，异常清单 L3、L11）：
 * - 把“关闭窗口时缩到托盘”和托盘菜单文字同步给 Rust 端；
 * - 收到退出请求（托盘“退出”、或关掉缩到托盘后关闭窗口）时，先确认收到（否则 Rust 端几秒后直接退出），
 *   再查询未同步文件数和打开的终端数；没有任何会中断的内容时直接退出，否则弹出确认框。
 */
export function ExitGuard() {
  const { t, i18n } = useTranslation();
  const { settings } = useSettings();
  const [open, setOpen] = useState(false);
  const [summary, setSummary] = useState<ExitSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tray, setTray] = useState(false);
  const [quitting, setQuitting] = useState(false);
  /** 每次退出请求的序号：旧请求的查询结果晚到时丢弃 */
  const seq = useRef(0);

  useEffect(() => { void trayAvailable().then(setTray); }, []);

  // 设置或语言变化时同步给外壳
  useEffect(() => {
    if (!inTauri) return;
    void shell()
      .then((c) => c.appShellConfig(settings.closeToTray, {
        open: t("tray.open"), quit: t("tray.quit"), syncPage: t("tray.syncPage"), pauseAll: t("tray.pauseAll"), resumeAll: t("tray.resumeAll"),
        // 状态一行是模板：{count}、{n} 由外壳填入（这里不让 i18next 插值）
        statusNone: t("tray.statusNone"), statusSynced: t("tray.statusSynced"), statusSyncing: t("tray.statusSyncing"),
        statusPaused: t("tray.statusPaused"), statusProblem: t("tray.statusProblem"), statusStopped: t("tray.statusStopped"), statusUnavailable: t("tray.statusUnavailable"),
      }))
      .catch(() => { /* 外壳保持默认：缩到托盘、中文菜单 */ });
  }, [settings.closeToTray, t, i18n.language]);

  const quit = useCallback(() => {
    setQuitting(true);
    void shell().then((c) => c.appQuit()).catch(() => setQuitting(false));
  }, []);

  const onRequest = useCallback((request: number) => {
    void shell().then((c) => c.appExitAck(request)).catch(() => {});
    const my = ++seq.current;
    // 确认框已经打开时保留上次的结果，只在后台刷新，避免闪回“正在检查”
    setOpen(true);
    setError(null);
    void shell()
      .then((c) => c.appExitSummary())
      .then((r) => {
        if (my !== seq.current) return;
        if (r.status === "error") throw r.error;
        const s = r.data as ExitSummary;
        if (nothingToLose(s)) quit();
        else setSummary(s);
      })
      .catch((e: unknown) => {
        if (my !== seq.current) return;
        setError(formatError(e));
      });
  }, [quit]);

  useEffect(() => {
    if (!inTauri) return;
    let alive = true;
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/event")
      .then(({ listen }) => listen<number>(EXIT_EVENT, (e) => onRequest(e.payload)))
      .then((u) => { if (alive) unlisten = u; else u(); });
    return () => { alive = false; unlisten?.(); };
  }, [onRequest]);

  if (!open) return null;
  const cancel = () => { seq.current++; setOpen(false); setSummary(null); };
  const toTray = () => { cancel(); void shell().then((c) => c.appHideToTray()).catch(() => {}); };
  return <ExitDialog summary={summary} error={error} tray={tray} quitting={quitting} onCancel={cancel} onTray={toTray} onQuit={quit} />;
}

/** 退出确认框（纯展示，开发预览也用它） */
export function ExitDialog({ summary, error, tray, quitting, onCancel, onTray, onQuit }: {
  summary: ExitSummary | null;
  error: string | null;
  tray: boolean;
  quitting: boolean;
  onCancel: () => void;
  onTray: () => void;
  onQuit: () => void;
}) {
  const { t } = useTranslation();
  const totalFiles = summary?.tasks.reduce((n, x) => n + x.needFiles, 0) ?? 0;
  const syncAffected = !!error || (!!summary && (summary.tasks.length > 0 || summary.syncUnknown));
  return (
    <Dialog
      title={t("exit.title")}
      danger
      onDismiss={quitting ? undefined : onCancel}
      actions={<>
        <Button onClick={onCancel} disabled={quitting}>{t("common.cancel")}</Button>
        {tray && syncAffected && <Button onClick={onTray} disabled={quitting}>{t("exit.toTray")}</Button>}
        <Button variant="danger" onClick={onQuit} disabled={quitting}>{t("exit.quit")}</Button>
      </>}
    >
      <div className="exit-summary">
        {!summary && !error && <span role="status" className="exit-summary__muted">{t("exit.checking")}</span>}
        {error && <Banner tone="warn" title={t("exit.unknown")}>{error}</Banner>}
        {summary && summary.tasks.length > 0 && (
          <>
            <strong>{t("exit.unsynced", { count: totalFiles })}</strong>
            <ul className="exit-summary__list">
              {summary.tasks.map((x, i) => (
                <li key={`${i}-${x.name}`}>{t("exit.taskLine", { name: x.name, count: x.needFiles, size: formatBytes(x.needBytes) })}</li>
              ))}
            </ul>
          </>
        )}
        {summary?.syncUnknown && <p>{t("exit.unknown")}</p>}
        {syncAffected && <p className="exit-summary__muted">{t("exit.syncStops")}{tray ? ` ${t("exit.toTrayHint")}` : ""}</p>}
        {summary && summary.terminals > 0 && <p>{t("exit.terminals", { count: summary.terminals })}</p>}
      </div>
    </Dialog>
  );
}
