import { useEffect, useState } from "react";
import { inTauri } from "../shell/TitleBar";

/** 装桌面的进度：从安装终端的输出里读出来，显示在桌面卡片上 */
export interface InstallProgress {
  /** update 更新软件源；download 下载软件包；install 安装；error 出错停了 */
  stage: "update" | "download" | "install" | "error";
  /** 下载到第几个包 */
  got: number;
  /** 一共要装几个包（apt 报的 newly installed + upgraded）；还不知道时为 null */
  total: number | null;
  /** 安装进度百分比（apt 的 Progress: [ 37%]）；没有时为 null */
  percent: number | null;
  /** 终端最后一行输出（去掉控制字符）；出错时是报错那行 */
  line: string;
}

const ORDER = { update: 0, download: 1, install: 2, error: 3 } as const;

/** 去掉终端控制序列（颜色、光标移动、标题等），回车按换行算 */
function plain(text: string): string {
  return text
    .replace(/\x1b\][^\x07\x1b]*(\x07|\x1b\\)/g, "")
    .replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "\n")
    // 保存、恢复光标（apt 画底部进度条前后用）按换行算，进度条和后面的输出分开
    .replace(/\x1b[78]/g, "\n")
    .replace(/\x1b[()][0-9A-Za-z]|\x1b[=>DEHMc]/g, "")
    .replace(/\r/g, "\n");
}

/**
 * 根据安装终端最新的一段输出推进进度。输出是滚动的末尾一段（前后两次会重叠），
 * 所以下载数取看到过的最大编号、百分比取最新的，阶段只往前走（出错除外）。
 * @param prev 上一次的进度；刚开始时为 null
 * @param tail 终端输出的末尾一段
 * @returns 新的进度
 */
export function nextProgress(prev: InstallProgress | null, tail: string): InstallProgress {
  const p: InstallProgress = prev ? { ...prev } : { stage: "update", got: 0, total: null, percent: null, line: "" };
  const text = plain(tail);
  const lines = text.split("\n").map((l) => l.trim()).filter(Boolean);
  const raise = (stage: InstallProgress["stage"]) => { if (ORDER[stage] > ORDER[p.stage]) p.stage = stage; };
  // apt 的汇总行：“0 upgraded, 151 newly installed, 0 to remove ...”，之后的 Get: 才是装桌面要下载的包
  const summary = /(\d+) upgraded, (\d+) newly installed/.exec(text);
  let downloads = text;
  if (summary) {
    p.total = Number(summary[1]) + Number(summary[2]);
    downloads = text.slice(summary.index);
    raise("download");
  }
  if (p.stage !== "update") {
    for (const m of downloads.matchAll(/^Get:(\d+)\s/gm)) p.got = Math.max(p.got, Number(m[1]));
  }
  if (/^(Unpacking|Setting up|Selecting previously unselected) /m.test(text)) raise("install");
  const progress = [...text.matchAll(/Progress: \[\s*(\d+)%\]/g)].pop();
  if (progress) {
    p.percent = Number(progress[1]);
    raise("install");
  }
  // 报错：apt 的 E: 行、sudo 拒绝
  const error = [...lines].reverse().find((l) => /^E: /.test(l) || /incorrect password attempts|is not in the sudoers file|not allowed to execute/.test(l));
  if (error) {
    p.stage = "error";
    p.line = error;
    return p;
  }
  const last = [...lines].reverse().find((l) => !/^Progress: \[/.test(l) && !/^\[#*\.*\]$/.test(l));
  if (last) p.line = last.length > 160 ? `${last.slice(0, 157)}…` : last;
  return p;
}

/** 主窗口把进度发给这台服务器的独立桌面窗口用的事件 */
const EVENT = "pierlink://desktop-install-progress";
const latest = new Map<string, InstallProgress>();
const listeners = new Map<string, Set<(p: InstallProgress | null) => void>>();
let lastEmit = 0;

/**
 * 安装终端有了新输出：更新这台服务器的进度，通知卡片（同一窗口直接通知，独立桌面窗口发事件）
 * @param serverId 服务器 ID
 * @param tail 终端输出的末尾一段
 */
export function reportInstallOutput(serverId: string, tail: string) {
  const p = nextProgress(latest.get(serverId) ?? null, tail);
  latest.set(serverId, p);
  listeners.get(serverId)?.forEach((l) => l(p));
  // 发给独立窗口的最多每 0.3 秒一次（apt 输出很快）；出错马上发
  const now = Date.now();
  if (!inTauri || (now - lastEmit < 300 && p.stage !== "error")) return;
  lastEmit = now;
  void import("@tauri-apps/api/event").then(({ emitTo }) => emitTo(`desktop-${serverId}`, EVENT, { serverId, progress: p })).catch(() => undefined);
}

/** 重新开始安装：清掉上一次的进度 */
export function resetInstallProgress(serverId: string) {
  latest.delete(serverId);
  listeners.get(serverId)?.forEach((l) => l(null));
}

/**
 * 这台服务器装桌面的进度（主窗口里直接拿，独立桌面窗口里收主窗口发来的事件）；还没有输出时为 null
 * @param serverId 服务器 ID
 * @param active 正在安装（不在安装时不订阅）
 */
export function useInstallProgress(serverId: string, active: boolean): InstallProgress | null {
  const [progress, setProgress] = useState<InstallProgress | null>(() => (active ? latest.get(serverId) ?? null : null));
  useEffect(() => {
    if (!active) { setProgress(null); return; }
    setProgress(latest.get(serverId) ?? null);
    // 输出很快：合并到下一帧再更新界面
    let frame = 0;
    let pending: InstallProgress | null = null;
    const update = (p: InstallProgress | null) => {
      pending = p;
      if (!frame) frame = requestAnimationFrame(() => { frame = 0; setProgress(pending); });
    };
    let set = listeners.get(serverId);
    if (!set) { set = new Set(); listeners.set(serverId, set); }
    set.add(update);
    let off: (() => void) | undefined;
    let alive = true;
    if (inTauri) {
      void import("@tauri-apps/api/event").then(({ listen }) => listen<{ serverId: string; progress: InstallProgress }>(EVENT, (e) => { if (e.payload.serverId === serverId) update(e.payload.progress); }))
        .then((u) => { if (alive) off = u; else u(); }, () => undefined);
    }
    return () => { alive = false; set?.delete(update); off?.(); if (frame) cancelAnimationFrame(frame); };
  }, [serverId, active]);
  return progress;
}
