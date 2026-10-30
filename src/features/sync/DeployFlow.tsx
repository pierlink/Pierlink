import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { DeployDialog } from "./DeployDialog";
import { getDeployApi, type DeployEvent } from "./deployApi";
import type { DeployPhase, DeployReportView, DeployStepKey, DeployStepView } from "./types";

const STEPS: DeployStepKey[] = ["inspect", "upload", "configure", "start", "tunnel", "pair"];

function initialSteps(usesSystemd: boolean, t: (k: string) => string): DeployStepView[] {
  const detail: Record<DeployStepKey, string> = {
    inspect: "uname -m · /etc/os-release · systemctl --user",
    upload: "~/.local/share/pierlink/bin/ · SHA-256",
    configure: t("deploy.detailConfigure"),
    start: usesSystemd ? t("deploy.detailSystemd") : "nohup · " + t("deploy.detailNohup"),
    tunnel: t("deploy.detailLater"),
    pair: t("deploy.detailLater"),
  };
  return STEPS.map((key) => ({ key, detail: detail[key], state: key === "inspect" ? "done" : "pending" }));
}

/**
 * 部署流程：检查服务器 → 显示检查结果和步骤 → 用户授权后执行，进度实时更新。
 * 出错停在出错的那一步，可重试（重新检查后再执行，已完成的部分会被复用）。
 */
export function DeployFlow({ serverId, serverName, onClose }: { serverId: string; serverName: string; onClose: () => void }) {
  const { t } = useTranslation();
  const [report, setReport] = useState<(DeployReportView & { usesSystemd: boolean }) | null>(null);
  const [inspectError, setInspectError] = useState<string | null>(null);
  const [steps, setSteps] = useState<DeployStepView[]>([]);
  const [phase, setPhase] = useState<DeployPhase>("review");
  const alive = useRef(true);
  // 开发模式下组件会先挂载、卸载再挂载一次，所以挂载时要重新置为 true
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);

  const inspect = () => {
    setInspectError(null);
    setReport(null);
    void getDeployApi().then((api) => api.inspect(serverId)).then(
      (r) => { if (alive.current) { setReport(r); setSteps(initialSteps(r.usesSystemd, t)); setPhase("review"); } },
      (e: unknown) => { if (alive.current) setInspectError(formatError(e)); },
    );
  };
  useEffect(inspect, [serverId]); // eslint-disable-line react-hooks/exhaustive-deps

  const onEvent = (e: DeployEvent) => {
    if (!alive.current) return;
    setSteps((ss) => ss.map((s) => {
      if (s.key !== e.step) return s;
      if (e.kind === "progress") return { ...s, state: "running", progress: e.fraction };
      return { ...s, state: e.state, progress: e.state === "running" ? s.progress : undefined };
    }));
  };

  const start = async () => {
    setPhase("running");
    try {
      const api = await getDeployApi();
      await api.run(serverId, true, onEvent);
      if (alive.current) setPhase("done");
    } catch (e) {
      if (!alive.current) return;
      const text = formatError(e);
      setSteps((ss) => {
        // 把错误挂在失败（或正在进行）的那一步上
        const idx = Math.max(ss.findIndex((s) => s.state === "failed"), ss.findIndex((s) => s.state === "running"));
        const at = idx >= 0 ? idx : ss.findIndex((s) => s.state === "pending");
        return ss.map((s, i) => (i === at ? { ...s, state: "failed", error: text, progress: undefined } : s));
      });
      setPhase("failed");
    }
  };

  if (inspectError) {
    return (
      <Dialog title={t("deploy.title", { server: serverName })} danger onDismiss={onClose} actions={<>
        <Button onClick={onClose}>{t("common.cancel")}</Button>
        <Button variant="primary" onClick={inspect}>{t("common.retry")}</Button>
      </>}>
        <Banner tone="danger" title={t("deploy.inspectFailed")}>{inspectError}</Banner>
      </Dialog>
    );
  }
  if (!report) {
    return (
      <Dialog title={t("deploy.title", { server: serverName })} onDismiss={onClose} actions={<Button onClick={onClose}>{t("common.cancel")}</Button>}>
        <span role="status">{t("deploy.inspecting")}</span>
      </Dialog>
    );
  }
  return (
    <DeployDialog
      report={report}
      steps={steps}
      phase={phase}
      onStart={() => void start()}
      onRetry={inspect}
      onCancel={() => void getDeployApi().then((api) => api.cancel(serverId))}
      onClose={onClose}
    />
  );
}
