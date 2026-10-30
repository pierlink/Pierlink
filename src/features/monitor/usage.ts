import type { DiskUsage } from "./api";

/**
 * 磁盘已用和百分比，算法和 df 一样：已用 / (已用 + 可用)，不把给 root 预留的空间算成已用。
 * @param d 一块磁盘的用量
 * @returns 已用字节数、百分比（总量为 0 时为 null）、给系统预留的字节数（不到总量 1% 时为 0，不值得提）
 */
export function diskUsage(d: DiskUsage): { used: number; percent: number | null; reserved: number } {
  // 旧版本服务器组件没有 used：退回“总量 − 可用”
  const used = d.used > 0 ? d.used : Math.max(0, d.total - d.free);
  const base = d.used > 0 ? d.used + d.free : d.total;
  const reserved = d.used > 0 ? Math.max(0, d.total - d.used - d.free) : 0;
  return { used, percent: base > 0 ? (used / base) * 100 : null, reserved: reserved > d.total * 0.01 ? reserved : 0 };
}
