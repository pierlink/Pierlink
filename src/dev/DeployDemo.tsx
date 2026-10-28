import { useEffect, useRef, useState } from "react";
import { DeployDialog } from "../features/sync/DeployDialog";
import type { DeployPhase, DeployReportView, DeployStepView } from "../features/sync/types";

/** 开发预览：模拟一次部署流程（不连接服务器）。网址片段决定初始场景，见 DeployDemo 的 scenario。 */
const report: DeployReportView = {
  serverName: "gpu-dev",
  user: "dev",
  osName: "Ubuntu 24.04.1 LTS",
  arch: "x86_64",
  diskMb: 36,
  existing: "none",
  otherComputers: [],
  warnings: [{ kind: "linger", user: "dev" }],
  blocker: null,
};

const initialSteps: DeployStepView[] = [
  { key: "inspect", detail: "uname -m · /etc/os-release · systemctl --user", state: "done" },
  { key: "upload", detail: "~/.local/share/pierlink/bin/v2.1.7/x86_64/syncthing · SHA-256", state: "pending" },
  { key: "configure", detail: "~/.local/state/pierlink/<实例>/home · 管理接口：Unix 套接字（0700）", state: "pending" },
  { key: "start", detail: "systemd --user · pierlink-<实例>.service", state: "pending" },
  { key: "tunnel", detail: "ssh -N -L 127.0.0.1:<本地端口>:127.0.0.1:22114 -L …:<套接字>", state: "pending" },
  { key: "pair", detail: "交换设备 ID，只信任彼此", state: "pending" },
];

export function DeployDemo({ scenario }: { scenario: "review" | "running" | "failed" | "blocked" | "done" }) {
  const [phase, setPhase] = useState<DeployPhase>(scenario === "blocked" ? "review" : scenario);
  const [steps, setSteps] = useState<DeployStepView[]>(() => {
    if (scenario === "running") return initialSteps.map((s, i) => (i === 1 ? { ...s, state: "running", progress: 0.42 } : s));
    if (scenario === "failed") return initialSteps.map((s, i) => (i === 1 ? { ...s, state: "done" } : i === 2 ? { ...s, state: "done" } : i === 3 ? { ...s, state: "failed", error: "启动同步服务失败：服务器上的同步端口 22114 已被占用，已换端口重试仍失败" } : s));
    if (scenario === "done") return initialSteps.map((s) => ({ ...s, state: "done" }));
    return initialSteps;
  });
  const [open, setOpen] = useState(true);
  const timer = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => () => { if (timer.current) clearInterval(timer.current); }, []);

  const run = () => {
    setPhase("running");
    timer.current = setInterval(() => {
      setSteps((ss) => {
        const i = ss.findIndex((s) => s.state === "pending" || s.state === "running");
        if (i < 0) { if (timer.current) clearInterval(timer.current); setPhase("done"); return ss; }
        const s = ss[i];
        if (s.key === "upload" && (s.progress ?? 0) < 1) {
          return ss.map((x, j) => (j === i ? { ...x, state: "running", progress: Math.min(1, (x.progress ?? 0) + 0.2) } : x));
        }
        if (s.state === "pending") return ss.map((x, j) => (j === i ? { ...x, state: "running" } : x));
        return ss.map((x, j) => (j === i ? { ...x, state: "done", progress: undefined } : x));
      });
    }, 500);
  };

  if (!open) return null;
  return (
    <DeployDialog
      report={scenario === "blocked" ? { ...report, warnings: [], blocker: { kind: "instanceNewer", serverVersion: "v2.2.0", appVersion: "v2.1.7" } } : report}
      steps={steps}
      phase={phase}
      onStart={run}
      onRetry={() => { setSteps((ss) => ss.map((s) => (s.state === "failed" ? { ...s, state: "pending", error: undefined } : s))); run(); }}
      onCancel={() => { if (timer.current) clearInterval(timer.current); setPhase("failed"); }}
      onClose={() => setOpen(false)}
    />
  );
}
