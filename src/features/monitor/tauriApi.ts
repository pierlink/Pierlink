import { Channel } from "@tauri-apps/api/core";
import { commands, type MonitorEventDto_Deserialize } from "../../bindings/commands";
import type { MonitorApi, MonitorEvent, ResourceSample } from "./api";

/** tauri-specta 对可能失败的命令返回 {status, data|error}；这里统一转成“成功返回数据、失败抛出错误” */
function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 后端事件 → 界面事件（数字字段生成的类型可能为 null，补上默认值） */
function toEvent(e: MonitorEventDto_Deserialize): MonitorEvent {
  if (e.kind !== "sample") return e.kind === "reconnecting" ? { ...e, nextRetryAtMs: e.nextRetryAtMs ?? 0 } : e;
  const s = e.sample;
  const sample: ResourceSample = {
    atMs: s.atMs ?? 0,
    uptimeS: s.uptimeS ?? 0,
    load: [s.load[0] ?? 0, s.load[1] ?? 0, s.load[2] ?? 0],
    cpuPercent: s.cpuPercent ?? null,
    cpuCores: s.cpuCores,
    netRxBps: s.netRxBps ?? null,
    netTxBps: s.netTxBps ?? null,
    nets: s.nets.map((n) => ({ name: n.name, rxBps: n.rxBps ?? null, txBps: n.txBps ?? null })),
    memTotal: s.memTotal ?? 0,
    memAvailable: s.memAvailable ?? 0,
    swapTotal: s.swapTotal ?? 0,
    swapFree: s.swapFree ?? 0,
    disks: s.disks.map((d) => ({ mount: d.mount, fsType: d.fsType, total: d.total ?? 0, used: d.used ?? 0, free: d.free ?? 0 })),
    gpus: s.gpus.map((g) => ({ index: g.index, name: g.name, utilPercent: g.utilPercent ?? null, memUsed: g.memUsed ?? null, memTotal: g.memTotal ?? null, tempC: g.tempC ?? null })),
    gpuError: s.gpuError ?? null,
    osName: s.osName ?? null,
  };
  return { kind: "sample", sample };
}

export const tauriMonitorApi: MonitorApi = {
  watch: async (serverId, onEvent, visible) => {
    const channel = new Channel<MonitorEventDto_Deserialize>();
    channel.onmessage = (e) => onEvent(toEvent(e));
    const id = unwrap(await commands.monitorWatch(serverId, visible, channel));
    return {
      stop: () => { void commands.monitorStop(id); },
      setVisible: (v) => { void commands.monitorVisible(id, v); },
    };
  },
};
