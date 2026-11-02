import type { DesktopDetection, DesktopInstallCheck } from "./api";

/** 精简桌面（只有 Openbox 和 xterm，不用管理员权限）的桌面环境 id，和后端一致 */
export const LIGHT_DESKTOP_ID = "pierlink-light";

/** 装桌面时最多等多久（之后停止自动查找，用户装好后自己点“重新查找”） */
export const INSTALL_WAIT_MS = 30 * 60 * 1000;
/** 装桌面时每隔多久查一次装好没有 */
export const INSTALL_POLL_MS = 5000;

/**
 * 检测结果的卡片里给什么：create 新建桌面（有桌面环境、工具齐全）；install 安装桌面（新建不了、能用包管理器装）；
 * 两个都没有时只能手动安装
 * @param d 检测结果
 */
export function desktopActions(d: DesktopDetection): { create: boolean; install: boolean } {
  const create = d.canCreate && d.desktops.length > 0;
  return { create, install: !create && !!d.installCommand };
}

/**
 * 装完了没有：能新建桌面了，而且服务器上没在跑 apt、dpkg 等（同一次安装里 TigerVNC、字体可能还在装）
 * @param d 装的过程中重新检测的结果
 */
export function installFinished(d: DesktopDetection): boolean {
  return desktopActions(d).create && !d.packageBusy;
}

/**
 * 新建卡片里默认选的桌面环境：刚装好的 XFCE，没有就第一个
 * @param d 检测结果
 * @returns 桌面环境 id；没有桌面环境时为空字符串
 */
export function preferredDesktop(d: DesktopDetection): string {
  return (d.desktops.find((e) => /xfce/i.test(`${e.id} ${e.name}`)) ?? d.desktops[0])?.id ?? "";
}

/** 安装确认框的状态 */
export type InstallStatus =
  /** 正在检查服务器 */
  | { kind: "checking" }
  /** 没能检查（error 是原因） */
  | { kind: "failed" }
  /** 没有管理员权限：给命令让管理员运行；light 为能改用精简桌面 */
  | { kind: "noAdmin"; light: boolean }
  /** 空间不够：disks 是不够的那些 */
  | { kind: "noSpace"; disks: DesktopInstallCheck["disks"] }
  /** 可以装：lowSpec 提醒配置低，password 为要在终端里输 sudo 密码 */
  | { kind: "ready"; lowSpec: boolean; password: boolean };

/**
 * 按检查结果决定确认框显示什么、能不能装。没有管理员权限最先说（空间够不够都装不了）；
 * 其次空间不够（不让装）；能装时再看配置低不低、要不要输密码
 * @param check 检查结果；还在检查时为 null
 * @param failed 检查失败了
 */
export function installStatus(check: DesktopInstallCheck | null, failed: boolean): InstallStatus {
  if (failed) return { kind: "failed" };
  if (!check) return { kind: "checking" };
  if (check.admin === "none") return { kind: "noAdmin", light: check.lightAvailable };
  if (!check.enoughSpace) return { kind: "noSpace", disks: check.disks.filter((d) => d.available < d.need) };
  return { kind: "ready", lowSpec: check.lowSpec, password: check.admin === "password" };
}
