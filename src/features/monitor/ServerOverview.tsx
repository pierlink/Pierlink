import { useState } from "react";
import { useTranslation } from "react-i18next";
import { setOverviewAuto, useOverviewAuto } from "./overviewPref";
import { Button, Select, Switch } from "../../components";
import { formatBytes } from "../sync/format";
import { passwordBus } from "../passwords/bus";
import type { ResourceSample } from "./api";
import { diskUsage } from "./usage";
import type { MonitorState } from "./useServerMonitor";
import "./monitor.css";

/** 记住每台服务器选的网卡 */
const NIC_KEY = "pierlink.overview.nic";
/** 全部网卡加起来 */
const ALL = "";

function loadNic(serverId: string): string {
  try {
    const all = JSON.parse(localStorage.getItem(NIC_KEY) ?? "{}") as Record<string, string>;
    return typeof all[serverId] === "string" ? all[serverId] : ALL;
  } catch {
    return ALL;
  }
}

function saveNic(serverId: string, nic: string) {
  try {
    const all = JSON.parse(localStorage.getItem(NIC_KEY) ?? "{}") as Record<string, string>;
    all[serverId] = nic;
    localStorage.setItem(NIC_KEY, JSON.stringify(all));
  } catch {
    /* 存不了就只在这次有效 */
  }
}

/** 一个小用量格：名称、百分比、细条；详情放悬停提示。≥80% 变黄 */
function Meter({ label, short, percent, value, detail, sub }: { label: string; short?: string; percent: number | null; value: string; detail: string; sub?: string }) {
  const p = percent === null ? 0 : Math.max(0, Math.min(100, percent));
  const warn = percent !== null && percent >= 80;
  return (
    <div className="ov-meter" title={detail}>
      <div className="ov-meter__head">
        <span className="ov-meter__label">{short ?? label}</span>
        <span className={`ov-meter__value${warn ? " res__value--warn" : ""}`}>{value}</span>
      </div>
      <div className="res__bar" role="meter" aria-label={label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent === null ? undefined : Math.round(p)}>
        <span className={warn ? "res__fill res__fill--warn" : "res__fill"} style={{ width: `${p}%` }} />
      </div>
      {/* 已用 / 总量；没有时占位，四格高度一致 */}
      <span className="ov-meter__sub">{sub ?? "\u00a0"}</span>
    </div>
  );
}

/** 已用 / 总量，单位只写一次：“5.4/7.8 GB”；总量为 0 或不知道时为 “—” */
function usedOfTotal(used: number | null, total: number | null): string {
  if (used === null || total === null || total <= 0) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let scale = 1;
  while (total / scale >= 1024 && i < units.length - 1) { scale *= 1024; i++; }
  const fmt = (v: number) => { const x = v / scale; return x >= 100 ? Math.round(x).toString() : x.toFixed(1); };
  return `${fmt(used)}/${fmt(total)} ${units[i]}`;
}

/** 每秒字节数 → “2.4 MB/s” */
function rate(bps: number): string {
  return `${formatBytes(bps)}/s`;
}

/** 运行时间：天、小时 */
function uptime(t: (k: string, o?: Record<string, unknown>) => string, s: number): string {
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  return d > 0 ? t("monitor.uptimeDays", { d, h }) : t("monitor.uptimeHours", { h, m: Math.floor((s % 3600) / 60) });
}

/** CPU、内存、交换、GPU 四格 */
function Meters({ sample }: { sample: ResourceSample }) {
  const { t } = useTranslation();
  const pct = (used: number, total: number) => (total > 0 ? (used / total) * 100 : null);
  const mem = pct(sample.memTotal - sample.memAvailable, sample.memTotal);
  const swap = sample.swapTotal > 0 ? pct(sample.swapTotal - sample.swapFree, sample.swapTotal) : null;
  const gpus = sample.gpus;
  const known = gpus.filter((g) => g.utilPercent !== null);
  const gpu = known.length ? known.reduce((n, g) => n + (g.utilPercent ?? 0), 0) / known.length : null;
  const gpuDetail = gpus.length === 0
    ? (sample.gpuError ? t("monitor.gpuError", { text: sample.gpuError }) : t("monitor.gpuNone"))
    : gpus.map((g) => [`GPU ${g.index}`, g.utilPercent === null ? "—" : `${g.utilPercent}%`, g.memUsed !== null && g.memTotal !== null ? t("monitor.gpuMem", { used: formatBytes(g.memUsed), total: formatBytes(g.memTotal) }) : null, g.tempC !== null ? `${g.tempC}°C` : null, g.name].filter(Boolean).join(" · ")).join("\n");
  return (
    <div className="ov-meters">
      <Meter label="CPU" percent={sample.cpuPercent} value={sample.cpuPercent === null ? "…" : `${Math.round(sample.cpuPercent)}%`}
        detail={t("monitor.cpuDetail", { cores: sample.cpuCores, load: sample.load[0].toFixed(2) })} />
      <Meter label={t("monitor.memory")} short={t("monitor.memoryShort")} percent={mem} value={`${Math.round(mem ?? 0)}%`}
        detail={`${formatBytes(sample.memTotal - sample.memAvailable)} / ${formatBytes(sample.memTotal)}`}
        sub={usedOfTotal(sample.memTotal - sample.memAvailable, sample.memTotal)} />
      <Meter label={t("monitor.swap")} percent={swap} value={swap === null ? "—" : `${Math.round(swap)}%`}
        detail={sample.swapTotal > 0 ? `${formatBytes(sample.swapTotal - sample.swapFree)} / ${formatBytes(sample.swapTotal)}` : t("monitor.swapNone")}
        sub={sample.swapTotal > 0 ? usedOfTotal(sample.swapTotal - sample.swapFree, sample.swapTotal) : t("monitor.swapOff")} />
      <Meter label={gpus.length > 1 ? t("monitor.gpuCount", { count: gpus.length }) : "GPU"} percent={gpu} value={gpu === null ? "—" : `${Math.round(gpu)}%`} detail={gpuDetail}
        sub={gpuMemory(gpus)} />
    </div>
  );
}

/** 显存：所有卡加起来的已用 / 总量；有一张卡读不到显存就不显示，免得数字偏小 */
function gpuMemory(gpus: ResourceSample["gpus"]): string | undefined {
  if (gpus.length === 0) return undefined;
  if (gpus.some((g) => g.memUsed === null || g.memTotal === null)) return "—";
  return usedOfTotal(gpus.reduce((n, g) => n + (g.memUsed ?? 0), 0), gpus.reduce((n, g) => n + (g.memTotal ?? 0), 0));
}

/** 每个挂载点一行：路径、细条、百分比、已用/总量 */
function Disks({ sample }: { sample: ResourceSample }) {
  const { t } = useTranslation();
  if (sample.disks.length === 0) return <p className="ov-muted" title={t("monitor.diskUnknown")}>{t("monitor.diskUnknown")}</p>;
  const disks = [...sample.disks].sort((a, b) => (a.mount === "/" ? -1 : b.mount === "/" ? 1 : a.mount.localeCompare(b.mount)));
  return (
    <div className="ov-disks" aria-label={t("monitor.diskNone")}>
      {disks.map((d) => {
        const u = diskUsage(d);
        const warn = u.percent !== null && u.percent >= 80;
        const p = u.percent === null ? 0 : Math.max(0, Math.min(100, u.percent));
        return (
          <div key={d.mount} className="ov-disk" title={`${d.mount}（${d.fsType}）\n${t("monitor.diskDetail", { used: formatBytes(u.used), total: formatBytes(d.total), free: formatBytes(d.free) })}${u.reserved ? `\n${t("monitor.diskReserved", { reserved: formatBytes(u.reserved) })}` : ""}`}>
            <span className="ov-disk__mount mono pl-ellipsis">{d.mount}</span>
            <span className="res__bar ov-disk__bar" role="meter" aria-label={t("monitor.disk", { mount: d.mount })} aria-valuemin={0} aria-valuemax={100} aria-valuenow={u.percent === null ? undefined : Math.round(p)}><span className={warn ? "res__fill res__fill--warn" : "res__fill"} style={{ width: `${p}%` }} /></span>
            <span className={`ov-disk__pct${warn ? " res__value--warn" : ""}`}>{u.percent === null ? "—" : `${Math.round(p)}%`}</span>
            <span className="ov-disk__size">{formatBytes(d.total)}</span>
          </div>
        );
      })}
    </div>
  );
}

/** 网络：选网卡（默认全部加起来），显示收发速度 */
function Network({ serverId, sample }: { serverId: string; sample: ResourceSample }) {
  const { t } = useTranslation();
  const [nic, setNic] = useState(() => loadNic(serverId));
  const picked = nic === ALL ? null : sample.nets.find((n) => n.name === nic) ?? null;
  // 选过的网卡这次没出现（被删了、改名了）：按全部显示，但不改记住的选择
  const rx = picked ? picked.rxBps : sample.netRxBps;
  const tx = picked ? picked.txBps : sample.netTxBps;
  const options = [{ value: ALL, label: t("monitor.nicAll") }, ...sample.nets.map((n) => ({ value: n.name, label: n.name }))];
  return (
    <div className="ov-net">
      <span className="ov-net__label">{t("monitor.network")}</span>
      <Select className="ov-net__nic" label={t("monitor.nicPick")} value={picked ? nic : ALL} options={options}
        onChange={(v) => { setNic(v); saveNic(serverId, v); }} />
      <span className="ov-net__rates">
        {rx === null || tx === null ? t("monitor.networkPending") : t("monitor.networkRates", { rx: rate(rx), tx: rate(tx) })}
      </span>
    </div>
  );
}

/**
 * 服务器终端右上角的概况：CPU、内存、交换、GPU，每个挂载点的磁盘，按网卡的网速。
 * 浮在终端上面，不挤动终端；右上角是“连接时是否自动显示”的开关。
 */
export function ServerOverview({ serverId, sample, state, onLogin }: {
  serverId: string;
  sample: ResourceSample | null;
  state: MonitorState;
  onLogin: () => void;
}) {
  const { t } = useTranslation();
  const auto = useOverviewAuto();
  return (
    <section className="ov" aria-label={t("monitor.title")}>
      <div className="ov__head">
        <span className="ov__summary pl-ellipsis" title={sample?.osName ?? undefined}>
          {sample
            ? [uptime(t, sample.uptimeS), sample.osName].filter(Boolean).join(" · ")
            : t("monitor.title")}
        </span>
        {/* 连接服务器时是否自动显示概况；这次显不显示用终端上方的概况按钮 */}
        <span className="ov__auto" title={t("monitor.autoTitle")}>
          {t("monitor.auto")}
          <Switch label={t("monitor.autoTitle")} checked={auto} onChange={setOverviewAuto} />
        </span>
      </div>
      {state.kind === "unsupported" && <p className="ov-muted">{t("monitor.unsupported")}</p>}
      {state.kind === "failed" && <p className="ov-warn" title={state.text}>{t("monitor.failed", { text: state.text })}</p>}
      {state.kind === "retry" && <p className="ov-warn pl-ellipsis" title={state.text}>{t("monitor.retrying", { text: state.text })}</p>}
      {state.kind === "login" && (
        <div className="ov-action">
          <p className="ov-warn">{state.reason ?? t("monitor.needLogin")}</p>
          {state.error && state.error !== state.reason && <p className="ov-warn">{state.error}</p>}
          <Button size="sm" disabled={state.busy} onClick={onLogin}>{state.reason ? t("terminal.reconnect") : t("servers.login")}</Button>
        </div>
      )}
      {state.kind === "password" && (
        <div className="ov-action">
          <p className="ov-warn">{state.wrong ? t("passwords.wrong") : t("monitor.needPassword")}</p>
          <Button size="sm" onClick={() => passwordBus.requestEntry(serverId)}>{t("passwords.enter")}</Button>
        </div>
      )}
      {!sample && (state.kind === "waiting" || state.kind === "connecting") && <p className="ov-muted">{t("monitor.connecting")}</p>}
      {sample && (
        <>
          <Meters sample={sample} />
          <Disks sample={sample} />
          <Network serverId={serverId} sample={sample} />
        </>
      )}
    </section>
  );
}
