import { useEffect, useState } from "react";
import { useInstallProgress } from "./installProgress";
import { useTranslation } from "react-i18next";
import { Button, Checkbox, Dialog } from "../../components";
import { formatError } from "../../errors";
import { CommandLine } from "../mcp/McpParts";
import { writeClipboardText } from "../shell/clipboard";
import { formatBytes } from "../sync/format";
import { getServersApi, notifyServersChanged } from "../servers/api";
import { getDesktopApi, type DesktopInstallCheck } from "./api";
import { installStatus } from "./install";

/** 安装时终端里要做什么：terminal 在下面的终端里装；password 还要输 sudo 密码；copied 命令已复制，用户自己找终端运行 */
export type InstallHint = "terminal" | "password" | "copied";

/**
 * 安装桌面的确认框。打开就检查服务器；检查中各项先占好位置，结果出来只替换文字，框不跳动。
 * 显示要下载、占用多少，磁盘剩余，内存和核数；空间不够时不能装并写原因，配置低时提醒一行；
 * 没有管理员权限时给命令让管理员运行，能退到精简桌面时给“用精简桌面”。
 * Chrome 浏览器可以选着一起装：安装命令一直显示（可以复制自己运行），勾上时接在桌面的安装命令后面一起运行。
 * @param command 检测结果里的安装命令（检查完以检查结果里的为准，两者一致）
 * @param chrome 下载并安装 Chrome 的命令；装不了（不是 x86_64、不认识包管理器）为 null，不显示这一项
 * @param root 登录的就是 root（打开 Chrome 要加 --no-sandbox）
 * @param inTerminal 能在桌面下面开终端运行；不能时（独立窗口）改为复制命令
 * @param onInstall 确认安装：要运行的命令、要不要输 sudo 密码
 * @param onLight 改用精简桌面
 */
export function InstallDialog({ serverId, command, chrome, root, inTerminal, onInstall, onLight, onClose }: {
  serverId: string;
  command: string;
  chrome: string | null;
  root: boolean;
  inTerminal: boolean;
  onInstall: (command: string, password: boolean) => void;
  onLight: () => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [check, setCheck] = useState<DesktopInstallCheck | null>(null);
  const [error, setError] = useState<string | null>(null);
  /** 点“重试”加一，重新检查 */
  const [round, setRound] = useState(0);
  /** 勾了“同时安装 Chrome” */
  const [withChrome, setWithChrome] = useState(false);
  // 这台服务器用系统 OpenSSH 连接时提示换成内置 SSH（开终端自动输入命令、连接进度都靠内置 SSH 更稳）
  const [engine, setEngineState] = useState<"builtin" | "openssh" | "switched" | null>(null);
  useEffect(() => {
    let alive = true;
    void getServersApi().then((api) => api.list()).then(
      (list) => { if (alive) setEngineState(list.find((s) => s.id === serverId)?.connectionEngine ?? null); },
      () => undefined,
    );
    return () => { alive = false; };
  }, [serverId]);
  const switchToBuiltin = async () => {
    try {
      await (await getServersApi()).setEngine(serverId, "builtin", null);
      notifyServersChanged();
      setEngineState("switched");
    } catch (e) {
      setError(formatError(e));
    }
  };
  useEffect(() => {
    let alive = true;
    setCheck(null);
    setError(null);
    void getDesktopApi().then((api) => api.installCheck(serverId)).then(
      (c) => { if (alive) setCheck(c); },
      (e: unknown) => { if (alive) setError(formatError(e)); },
    );
    return () => { alive = false; };
  }, [serverId, round]);
  const status = installStatus(check, error !== null);
  const shown = check?.command ?? command;
  const pending = error === null ? "…" : "—";
  const size = (bytes: number) => (check?.estimated ? t("desktop.install.about", { size: formatBytes(bytes) }) : formatBytes(bytes));
  const rows: [string, string][] = check
    ? [
        [t("desktop.install.download"), size(check.download)],
        [t("desktop.install.installed"), size(check.installed)],
        [t("desktop.install.free"), check.disks.map((d) => t("desktop.install.onMount", { size: formatBytes(d.available), mount: d.mount })).join(" · ")],
        [t("desktop.install.memory"), check.memory ? t("desktop.install.memoryValue", { total: formatBytes(check.memory), available: formatBytes(check.availableMemory) }) : "—"],
        [t("desktop.install.cpu"), check.cpus ? t("desktop.install.cores", { count: check.cpus }) : "—"],
      ]
    : (["download", "installed", "free", "memory", "cpu"] as const).map((k) => [t(`desktop.install.${k}`), pending]);
  let note: { text: string; tone: "muted" | "warn" | "error" } | null = null;
  if (status.kind === "checking") note = { text: t("desktop.install.checking"), tone: "muted" };
  else if (status.kind === "failed") note = { text: t("desktop.install.failed", { reason: error }), tone: "error" };
  else if (status.kind === "noAdmin") note = { text: t("desktop.install.noAdmin"), tone: "warn" };
  else if (status.kind === "noSpace") {
    note = {
      text: status.disks.map((d) => t("desktop.install.noSpace", { mount: d.mount, available: formatBytes(d.available), need: formatBytes(d.need) })).join(t("desktop.sep")),
      tone: "error",
    };
  } else if (status.lowSpec) note = { text: t("desktop.install.lowSpec"), tone: "warn" };
  const password = status.kind === "ready" && status.password;
  const label = status.kind === "noAdmin" ? t("desktop.install.runAdmin")
    : !inTerminal ? t("desktop.install.runCopy")
      : password ? t("desktop.install.runPassword") : t("desktop.install.run");
  return (
    <Dialog title={t("desktop.install.title")} onDismiss={onClose}
      actions={<>
        <Button onClick={onClose}>{t("common.cancel")}</Button>
        {status.kind === "failed"
          ? <Button variant="primary" onClick={() => setRound((n) => n + 1)}>{t("common.retry")}</Button>
          : status.kind === "noAdmin"
            ? (status.light && <Button variant="primary" onClick={onLight}>{t("desktop.install.light")}</Button>)
            : (
              <Button variant="primary" disabled={status.kind !== "ready"} onClick={() => onInstall(withChrome && chrome ? `${shown} && ${chrome}` : shown, password)}>
                {inTerminal ? t("desktop.install.confirm") : t("desktop.install.copy")}
              </Button>
            )}
      </>}>
      <div className="desk-install" data-state={status.kind}>
        <dl className="desk-install__rows">
          {rows.map(([name, value]) => (
            <div key={name}><dt>{name}</dt><dd>{value}</dd></div>
          ))}
        </dl>
        <p className={`desk-install__note desk-install__note--${note?.tone ?? "muted"}`} role={note?.tone === "error" ? "alert" : "status"}>{note?.text ?? ""}</p>
        {engine === "openssh" && (
          <p className="desk-install__engine">
            <span>{t("desktop.install.openssh")}</span>
            <Button size="sm" onClick={() => void switchToBuiltin()}>{t("desktop.install.useBuiltin")}</Button>
          </p>
        )}
        {engine === "switched" && <p className="desk-install__engine"><span>{t("desktop.install.switched")}</span></p>}
        <p>{label}</p>
        <CommandLine text={shown} />
        {chrome && (
          <div className="desk-install__chrome">
            <Checkbox label={t("desktop.chrome.also")} checked={withChrome} onChange={(e) => setWithChrome(e.target.checked)} />
            <CommandLine text={chrome} />
            <p className="desk-install__hint">{t(root ? "desktop.chrome.runRoot" : "desktop.chrome.run")}</p>
          </div>
        )}
      </div>
    </Dialog>
  );
}

/**
 * 在桌面里用浏览器：怎么打开 Chrome；还没装时给安装命令（可以复制，能开终端时可以直接在下面的终端里装）。
 * @param command 下载并安装 Chrome 的命令
 * @param root 登录的就是 root（打开要加 --no-sandbox）
 * @param onInstall 在下面的终端里运行安装命令；不给（独立窗口里）时只能复制
 */
export function ChromeDialog({ command, root, onInstall, onClose }: {
  command: string;
  root: boolean;
  onInstall?: () => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  return (
    <Dialog title={t("desktop.chrome.title")} onDismiss={onClose}
      actions={<>
        <Button onClick={onClose}>{t("common.close")}</Button>
        {onInstall && <Button variant="primary" onClick={onInstall}>{t("desktop.chrome.installHere")}</Button>}
      </>}>
      <div className="desk-install">
        <p>{t(root ? "desktop.chrome.runRoot" : "desktop.chrome.run")}</p>
        <p className="desk-install__hint">{t("desktop.chrome.installHint")}</p>
        <CommandLine text={command} />
      </div>
    </Dialog>
  );
}

/**
 * 正在安装：提示看下面的终端（或运行复制的命令），可以再复制一次命令、点“重新查找”。
 * 等了太久（timedOut）时停下进度条，提示装好后自己点“重新查找”
 */
export function InstallingCard({ serverId, since, command, hint, timedOut, onRecheck }: {
  serverId: string;
  /** 开始安装的时间（毫秒） */
  since: number;
  command: string;
  hint: InstallHint;
  timedOut: boolean;
  onRecheck: () => void;
}) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const progress = useInstallProgress(serverId, true);
  // 已用时间每秒走一次
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  const seconds = Math.max(0, Math.floor((now - since) / 1000));
  const elapsed = t("desktop.install.elapsed", { min: Math.floor(seconds / 60), sec: String(seconds % 60).padStart(2, "0") });
  // 终端在等什么（sudo 密码、apt 被占着、命令没自动输入）就照实说；还没开始装时照原来的提示
  const waiting = progress?.stage === "error" ? null
    : progress?.waiting === "password" ? t("desktop.install.waitPassword")
      : progress?.waiting === "lock" ? t("desktop.install.waitLock")
        : progress?.waiting === "notTyped" ? t("desktop.install.typeYourself") : null;
  const stage = waiting ?? (!progress || progress.stage === "start" ? (timedOut ? t("desktop.install.notYet", { button: t("desktop.recheck") })
    : hint === "password" ? t("desktop.install.followPassword")
      : hint === "copied" ? t("desktop.install.followCopied") : t("desktop.install.follow"))
    : progress.stage === "error" ? t("desktop.install.stageError")
      : progress.stage === "install" ? (progress.percent !== null ? t("desktop.install.stageInstallPct", { pct: progress.percent }) : t("desktop.install.stageInstall"))
        : progress.stage === "download" ? (progress.total ? t("desktop.install.stageDownload", { got: progress.got, total: progress.total }) : t("desktop.install.stageDownloadStart"))
          : t("desktop.install.stageUpdate"));
  // 进度条：下载占前一半，安装占后一半；不知道进度时显示忙碌条
  const pct = !progress || progress.stage === "error" ? null
    : progress.stage === "install" ? (progress.percent !== null ? 50 + progress.percent / 2 : null)
      : progress.stage === "download" && progress.total ? Math.min(50, (progress.got / progress.total) * 50) : null;
  return (
    <div className="desk__card desk__card--installing">
      <b>{t("desktop.install.installing")}</b>
      <p role="status" className={progress?.stage === "error" ? "desk__error" : undefined}>{stage}</p>
      <div className="desk__progress" aria-hidden="true">
        {/* 在等用户（输密码、自己粘贴命令）时不显示忙碌条，免得看着像在装 */}
        {pct !== null ? <span className="desk__progress-bar" style={{ width: `${pct}%` }} />
          : !timedOut && progress?.stage !== "error" && progress?.waiting !== "password" && progress?.waiting !== "notTyped" && <span className="desk__progress-busy" />}
      </div>
      {/* 终端最后一行和已用时间：固定占两行，有没有输出都不跳 */}
      <p className="desk__install-line mono" title={progress?.line}>{progress?.line ?? ""}</p>
      <p className="desk__hint">{elapsed}</p>
      <div className="desk__actions">
        <Button onClick={() => { void writeClipboardText(command).then(() => setCopied(true), () => undefined); }}>
          {copied ? t("desktop.copied") : t("desktop.install.copy")}
        </Button>
        <Button variant="primary" onClick={onRecheck}>{t("desktop.recheck")}</Button>
      </div>
    </div>
  );
}
