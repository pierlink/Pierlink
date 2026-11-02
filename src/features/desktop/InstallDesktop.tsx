import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { CommandLine } from "../mcp/McpParts";
import { writeClipboardText } from "../shell/clipboard";
import { formatBytes } from "../sync/format";
import { getDesktopApi, type DesktopInstallCheck } from "./api";
import { installStatus } from "./install";

/** 安装时终端里要做什么：terminal 在下面的终端里装；password 还要输 sudo 密码；copied 命令已复制，用户自己找终端运行 */
export type InstallHint = "terminal" | "password" | "copied";

/**
 * 安装桌面的确认框。打开就检查服务器；检查中各项先占好位置，结果出来只替换文字，框不跳动。
 * 显示要下载、占用多少，磁盘剩余，内存和核数；空间不够时不能装并写原因，配置低时提醒一行；
 * 没有管理员权限时给命令让管理员运行，能退到精简桌面时给“用精简桌面”。
 * @param command 检测结果里的安装命令（检查完以检查结果里的为准，两者一致）
 * @param inTerminal 能在桌面下面开终端运行；不能时（独立窗口）改为复制命令
 * @param onInstall 确认安装：要运行的命令、要不要输 sudo 密码
 * @param onLight 改用精简桌面
 */
export function InstallDialog({ serverId, command, inTerminal, onInstall, onLight, onClose }: {
  serverId: string;
  command: string;
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
              <Button variant="primary" disabled={status.kind !== "ready"} onClick={() => onInstall(shown, password)}>
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
        <p>{label}</p>
        <CommandLine text={shown} />
      </div>
    </Dialog>
  );
}

/**
 * 正在安装：提示看下面的终端（或运行复制的命令），可以再复制一次命令、点“重新查找”。
 * 等了太久（timedOut）时停下进度条，提示装好后自己点“重新查找”
 */
export function InstallingCard({ command, hint, timedOut, onRecheck }: {
  command: string;
  hint: InstallHint;
  timedOut: boolean;
  onRecheck: () => void;
}) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const text = timedOut ? t("desktop.install.notYet", { button: t("desktop.recheck") })
    : hint === "password" ? t("desktop.install.followPassword")
      : hint === "copied" ? t("desktop.install.followCopied") : t("desktop.install.follow");
  return (
    <div className="desk__card desk__card--installing">
      <b>{t("desktop.install.installing")}</b>
      <p role="status">{text}</p>
      <div className="desk__progress" aria-hidden="true">{!timedOut && <span className="desk__progress-busy" />}</div>
      <div className="desk__actions">
        <Button onClick={() => { void writeClipboardText(command).then(() => setCopied(true), () => undefined); }}>
          {copied ? t("desktop.copied") : t("desktop.install.copy")}
        </Button>
        <Button variant="primary" onClick={onRecheck}>{t("desktop.recheck")}</Button>
      </div>
    </div>
  );
}
