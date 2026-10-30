import { inTauri } from "../shell/TitleBar";
import type { DeployReportView, DeployStepKey, DeployStepState } from "./types";

/** 部署进度事件（与后端 DeployEventDto 对应） */
export type DeployEvent =
  | { kind: "step"; step: DeployStepKey; state: DeployStepState }
  | { kind: "progress"; step: DeployStepKey; fraction: number };

export interface DeployApi {
  inspect(serverId: string): Promise<DeployReportView & { usesSystemd: boolean }>;
  run(serverId: string, upgradeConfirmed: boolean, onEvent: (e: DeployEvent) => void): Promise<void>;
  cancel(serverId: string): Promise<void>;
}

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

async function createTauriDeployApi(): Promise<DeployApi> {
  const [{ commands }, { Channel }] = await Promise.all([import("../../bindings/commands"), import("@tauri-apps/api/core")]);
  return {
    async inspect(serverId) {
      const r = unwrap(await commands.deployInspect(serverId));
      return { ...r, existing: r.existing as DeployReportView["existing"], warnings: r.warnings as DeployReportView["warnings"] };
    },
    async run(serverId, upgradeConfirmed, onEvent) {
      const ch = new Channel<DeployEvent>();
      ch.onmessage = onEvent;
      unwrap(await commands.deployRun(serverId, upgradeConfirmed, ch as never));
    },
    async cancel(serverId) { await commands.deployCancel(serverId); },
  };
}

/** 浏览器预览：模拟一次部署（不连接服务器） */
function createPreviewDeployApi(): DeployApi {
  let cancelled = false;
  const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
  return {
    async inspect() {
      await wait(500);
      return {
        serverName: "gpu-dev", user: "dev", osName: "Ubuntu 24.04.1 LTS", arch: "x86_64", diskMb: 36, existing: "none",
        otherComputers: [], warnings: [{ kind: "linger", user: "dev" }], blocker: null, usesSystemd: true,
      };
    },
    async run(_id, _u, onEvent) {
      cancelled = false;
      onEvent({ kind: "step", step: "inspect", state: "done" });
      onEvent({ kind: "step", step: "upload", state: "running" });
      for (let f = 0; f <= 1; f += 0.1) {
        if (cancelled) throw { code: "deploy.cancelled", params: {} };
        onEvent({ kind: "progress", step: "upload", fraction: f });
        await wait(150);
      }
      for (const s of ["upload", "configure", "start"] as DeployStepKey[]) {
        if (s !== "upload") { onEvent({ kind: "step", step: s, state: "running" }); await wait(400); }
        onEvent({ kind: "step", step: s, state: "done" });
      }
      onEvent({ kind: "step", step: "tunnel", state: "skipped" });
      onEvent({ kind: "step", step: "pair", state: "skipped" });
    },
    async cancel() { cancelled = true; },
  };
}

let api: Promise<DeployApi> | null = null;
export function getDeployApi(): Promise<DeployApi> {
  api ??= inTauri ? createTauriDeployApi() : Promise.resolve(createPreviewDeployApi());
  return api;
}
