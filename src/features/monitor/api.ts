import { inTauri } from "../shell/TitleBar";

/** used：df 的 Used 列（不含给 root 预留的部分，旧版本没有时为 0）；free：df 的 Avail 列 */
export interface DiskUsage { mount: string; fsType: string; total: number; used: number; free: number }
/** 一块网卡每秒接收/发送的字节数；第一次采样还没有差值，为 null */
export interface NetUsage { name: string; rxBps: number | null; txBps: number | null }
export interface GpuUsage { index: number; name: string; utilPercent: number | null; memUsed: number | null; memTotal: number | null; tempC: number | null }
/** 一次采样（字节数、秒数都已换成普通数字） */
export interface ResourceSample {
  atMs: number;
  uptimeS: number;
  load: [number, number, number];
  /** 第一次采样还没有差值，为 null */
  cpuPercent: number | null;
  cpuCores: number;
  /** 全部网卡（不含回环）每秒接收/发送的字节数；第一次采样还没有差值，为 null */
  netRxBps: number | null;
  netTxBps: number | null;
  /** 每块网卡（不含回环） */
  nets: NetUsage[];
  memTotal: number;
  memAvailable: number;
  /** 交换区总量和剩余（字节）；没开交换区时都是 0 */
  swapTotal: number;
  swapFree: number;
  disks: DiskUsage[];
  gpus: GpuUsage[];
  /** nvidia-smi 超时或出错的原因；没有 GPU 时为 null */
  gpuError: string | null;
  /** 系统名：只在每次连接的第一次采样里有，之后为 null（界面沿用上一次的） */
  osName: string | null;
}

export type MonitorEvent =
  | { kind: "connecting" }
  /** 这台服务器要密码登录，本次运行还没输过或输的不对：等用户输入（不计重试） */
  | { kind: "waitingPassword"; error: unknown }
  | { kind: "sample"; sample: ResourceSample }
  | { kind: "reconnecting"; attempt: number; nextRetryAtMs: number; error: unknown }
  | { kind: "unsupported"; reason: string }
  /** 认证失败等不再重试的错误 */
  | { kind: "failed"; error: unknown }
  | { kind: "stopped" };

/** 一次观察：停止、切换显示（隐藏时采样放慢但不停） */
export interface MonitorWatch {
  stop(): void;
  setVisible(visible: boolean): void;
}

/**
 * 服务器概况接口：同一台服务器的采样在应用里共用一份，新观察者先收到最近一帧。
 * visible 为概况是否显示：都隐藏时后端放慢采样
 */
export interface MonitorApi {
  watch(serverId: string, onEvent: (e: MonitorEvent) => void, visible: boolean): Promise<MonitorWatch>;
}

/** 浏览器预览：每 3 秒生成一组示例数据 */
function previewApi(): MonitorApi {
  return {
    watch: async (_serverId, onEvent, _visible) => {
      onEvent({ kind: "connecting" });
      let first = true;
      const gen = () => {
        const cpu = first ? null : 20 + Math.random() * 70;
        first = false;
        onEvent({
          kind: "sample",
          sample: {
            atMs: Date.now(), uptimeS: 12 * 86400 + 3600, load: [2.41, 2.1, 1.87], cpuPercent: cpu, cpuCores: 32,
            netRxBps: cpu === null ? null : 2.4 * 2 ** 20, netTxBps: cpu === null ? null : 310 * 2 ** 10,
            nets: [
              { name: "eth0", rxBps: cpu === null ? null : 2.3 * 2 ** 20, txBps: cpu === null ? null : 300 * 2 ** 10 },
              { name: "docker0", rxBps: cpu === null ? null : 80 * 2 ** 10, txBps: cpu === null ? null : 10 * 2 ** 10 },
            ],
            memTotal: 128 * 2 ** 30, memAvailable: 50 * 2 ** 30, swapTotal: 8 * 2 ** 30, swapFree: 6.5 * 2 ** 30,
            disks: [
              { mount: "/", fsType: "ext4", total: 1.8 * 2 ** 40, used: 1.21 * 2 ** 40, free: 0.5 * 2 ** 40 },
              { mount: "/data", fsType: "xfs", total: 7.3 * 2 ** 40, used: 6.4 * 2 ** 40, free: 0.9 * 2 ** 40 },
              { mount: "/boot", fsType: "ext4", total: 2 ** 30, used: 0.3 * 2 ** 30, free: 0.7 * 2 ** 30 },
            ],
            gpus: [{ index: 0, name: "NVIDIA A100 80GB PCIe", utilPercent: 87, memUsed: 62 * 2 ** 30, memTotal: 80 * 2 ** 30, tempC: 71 }],
            gpuError: null, osName: "Ubuntu 22.04.4 LTS",
          },
        });
      };
      const t0 = setTimeout(gen, 400);
      const timer = setInterval(gen, 1000);
      return { stop: () => { clearTimeout(t0); clearInterval(timer); }, setVisible: () => {} };
    },
  };
}

let api: Promise<MonitorApi> | null = null;
/** 取得资源概览接口（Tauri 里用应用后端，浏览器预览用示例数据） */
export function getMonitorApi(): Promise<MonitorApi> {
  api ??= inTauri ? import("./tauriApi").then((m) => m.tauriMonitorApi) : Promise.resolve(previewApi());
  return api;
}
