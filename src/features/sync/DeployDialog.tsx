import { useTranslation } from "react-i18next";
import { Check, X } from "lucide-react";
import { Banner, Button, Dialog } from "../../components";
import type { DeployPhase, DeployReportView, DeployStepView, DeployWarning } from "./types";
import "./sync.css";

interface Props {
  report: DeployReportView;
  steps: DeployStepView[];
  phase: DeployPhase;
  /** 用户授权并开始部署 */
  onStart: () => void;
  /** 从失败的那一步重试 */
  onRetry: () => void;
  /** 部署中取消（在两步之间生效） */
  onCancel: () => void;
  onClose: () => void;
}

/** 一条检查发现的情况 → 提示条 */
function WarningBanner({ w }: { w: DeployWarning }) {
  const { t } = useTranslation();
  switch (w.kind) {
    case "linger":
      return (
        <Banner tone="warn" title={t("deploy.warn.lingerTitle")}>
          {t("deploy.warn.lingerBody")}
          <code className="mono sync-cmd">{`sudo loginctl enable-linger ${w.user}`}</code>
        </Banner>
      );
    case "noSystemd":
      return <Banner tone="warn" title={t("deploy.warn.noSystemdTitle")}>{t(`deploy.warn.noSystemd.${w.reason}`)}</Banner>;
    case "killUserProcesses":
      return <Banner tone="warn" title={t("deploy.warn.killTitle")}>{t("deploy.warn.killBody")}</Banner>;
    case "networkFs":
      return <Banner tone="info" title={t("deploy.warn.networkFsTitle", { fs: w.fsType })}>{t("deploy.warn.networkFsBody")}</Banner>;
    case "ownSyncthing":
      return <Banner tone="info" title={t("deploy.warn.ownSyncthingTitle")}>{t("deploy.warn.ownSyncthingBody", { count: w.count })}</Banner>;
    case "lowInotify":
      return <Banner tone="info" title={t("deploy.warn.inotifyTitle")}>{t("deploy.warn.inotifyBody", { limit: w.limit })}</Banner>;
    case "lockHeld":
      return <Banner tone="info" title={t("deploy.warn.lockTitle")}>{t("deploy.warn.lockBody", { owner: w.owner })}</Banner>;
  }
}

function StepRow({ step, index }: { step: DeployStepView; index: number }) {
  const { t } = useTranslation();
  const mark =
    step.state === "done" ? <Check size={12} strokeWidth={3} aria-hidden /> :
    step.state === "failed" ? <X size={12} strokeWidth={3} aria-hidden /> :
    index + 1;
  return (
    <li className={`deploy-step deploy-step--${step.state}`}>
      <span className="deploy-step__mark">{mark}</span>
      <span className="deploy-step__main">
        <span className="deploy-step__title">{t(`deploy.step.${step.key}`)}</span>
        <span className="deploy-step__detail mono" title={step.detail}>{step.detail}</span>
        {step.state === "running" && step.progress !== undefined && (
          <span className="deploy-step__bar" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(step.progress * 100)}>
            <span style={{ width: `${Math.round(step.progress * 100)}%` }} />
          </span>
        )}
        {step.error && <span className="deploy-step__error" role="alert">{step.error}</span>}
      </span>
      <span className="deploy-step__state">
        {step.state === "running" && step.progress !== undefined ? `${Math.round(step.progress * 100)}%` : t(`deploy.state.${step.state}`)}
      </span>
    </li>
  );
}

/**
 * 部署服务器端同步组件：先列出检查结果和每一步要做的事，用户授权后逐步执行。
 * 出错时停在出错的那一步，可以重试；不会自动关闭。
 */
export function DeployDialog({ report, steps, phase, onStart, onRetry, onCancel, onClose }: Props) {
  const { t } = useTranslation();
  const blocked = report.blocker !== null;
  const title = report.existing === "upgrade"
    ? t("deploy.titleUpgrade", { server: report.serverName })
    : t("deploy.title", { server: report.serverName });

  const actions = (() => {
    if (phase === "running") return <Button onClick={onCancel}>{t("common.cancel")}</Button>;
    if (phase === "done") return <Button variant="primary" onClick={onClose}>{t("deploy.finish")}</Button>;
    if (phase === "failed") return <>
      <Button onClick={onClose}>{t("deploy.close")}</Button>
      <Button variant="primary" onClick={onRetry}>{t("common.retry")}</Button>
    </>;
    return <>
      <Button onClick={onClose}>{t("common.cancel")}</Button>
      {!blocked && <Button variant="primary" onClick={onStart}>{t("deploy.authorize")}</Button>}
    </>;
  })();

  return (
    <Dialog title={title} size="lg" onDismiss={phase === "running" ? undefined : onClose} actions={actions}>
      <div className="deploy">
        <p className="deploy__lead">{report.blocker ? t("deploy.leadBlocked") : t(report.existing === "none" ? "deploy.leadNew" : `deploy.lead.${report.existing}`)}</p>

        <div className="deploy__facts">
          <div><span>{t("deploy.fact.system")}</span><b>{report.osName ? `${report.osName} · ${report.arch}` : report.arch}</b></div>
          <div><span>{t("deploy.fact.disk")}</span><b>{t("deploy.fact.diskValue", { mb: report.diskMb })}</b></div>
          <div><span>{t("deploy.fact.memory")}</span><b>{t("deploy.fact.memoryValue")}</b></div>
        </div>

        {report.blocker && (
          <Banner tone="danger" title={t(`deploy.blocker.${report.blocker.kind}.title`)}>
            {t(`deploy.blocker.${report.blocker.kind}.body`, report.blocker)}
          </Banner>
        )}

        {!blocked && (
          <ol className="deploy__steps">
            {steps.map((s, i) => <StepRow key={s.key} step={s} index={i} />)}
          </ol>
        )}

        {report.otherComputers.length > 0 && (
          <Banner tone="warn" title={t("deploy.othersTitle")}>
            {t("deploy.othersBody", { list: report.otherComputers.join(t("common.listSep")) })}
          </Banner>
        )}
        {report.warnings.map((w) => <WarningBanner key={w.kind} w={w} />)}

        {phase === "review" && !blocked && <p className="deploy__consent">{t("deploy.consent")}</p>}
        {phase === "done" && <Banner tone="ok" title={t("deploy.doneTitle")}>{t("deploy.doneBody")}</Banner>}
      </div>
    </Dialog>
  );
}
