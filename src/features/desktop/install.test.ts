import { describe, expect, it } from "vitest";
import type { DesktopDetection, DesktopInstallCheck } from "./api";
import { desktopActions, installFinished, installStatus, preferredDesktop } from "./install";

const COMMAND = "sudo apt-get update && sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends xfce4";

/** 没有桌面环境、能用 apt 装的服务器 */
function bare(patch: Partial<DesktopDetection> = {}): DesktopDetection {
  return {
    sessions: [],
    desktops: [],
    canCreate: false,
    needsServerProgram: true,
    missingTools: [],
    officialOk: true,
    installCommand: COMMAND,
    lightAvailable: true,
    packageBusy: false,
    ...patch,
  };
}

/** 装好了 XFCE 的服务器 */
function withXfce(patch: Partial<DesktopDetection> = {}): DesktopDetection {
  return bare({ desktops: [{ id: "openbox", name: "Openbox", heavy: false }, { id: "xfce", name: "Xfce Session", heavy: false }], canCreate: true, needsServerProgram: false, ...patch });
}

/** 能装的检查结果：sudo 要密码、空间够、配置正常 */
function check(patch: Partial<DesktopInstallCheck> = {}): DesktopInstallCheck {
  return {
    admin: "password",
    memory: 1977936 * 1024,
    availableMemory: 1405808 * 1024,
    cpus: 2,
    download: 129_202_014,
    installed: 379_855 * 1024,
    estimated: false,
    disks: [{ mount: "/", available: 27_056_756 * 1024, need: 1_554_605 * 1024 }],
    enoughSpace: true,
    lowSpec: false,
    command: COMMAND,
    lightAvailable: true,
    ...patch,
  };
}

describe("检测结果的卡片给什么", () => {
  it("没有桌面环境时主操作是安装桌面，不是新建精简桌面", () => {
    expect(desktopActions(bare())).toEqual({ create: false, install: true });
  });

  it("有桌面环境、能新建时给新建", () => {
    expect(desktopActions(withXfce())).toEqual({ create: true, install: false });
  });

  it("有桌面环境但缺工具（不能新建）时也给安装", () => {
    expect(desktopActions(withXfce({ canCreate: false, missingTools: ["xauth"] }))).toEqual({ create: false, install: true });
  });

  it("不认识包管理器时两个都没有（只给手动安装说明）", () => {
    expect(desktopActions(bare({ installCommand: null }))).toEqual({ create: false, install: false });
  });
});

describe("装好了没有", () => {
  it("能新建了、也没在跑 apt 才算装好", () => {
    expect(installFinished(bare())).toBe(false);
    expect(installFinished(withXfce({ packageBusy: true }))).toBe(false);
    expect(installFinished(withXfce())).toBe(true);
  });

  it("服务器自带的 TigerVNC 太旧（还要另外获取）不影响算装好", () => {
    expect(installFinished(withXfce({ needsServerProgram: true }))).toBe(true);
  });

  it("装好后默认选 XFCE，没有就第一个", () => {
    expect(preferredDesktop(withXfce())).toBe("xfce");
    expect(preferredDesktop(bare({ desktops: [{ id: "mate", name: "MATE", heavy: false }] }))).toBe("mate");
    expect(preferredDesktop(bare())).toBe("");
  });
});

describe("安装确认框的状态", () => {
  it("检查中、检查失败", () => {
    expect(installStatus(null, false)).toEqual({ kind: "checking" });
    expect(installStatus(null, true)).toEqual({ kind: "failed" });
  });

  it("能装：sudo 要密码时说一声，root、免密 sudo 不用", () => {
    expect(installStatus(check(), false)).toEqual({ kind: "ready", lowSpec: false, password: true });
    expect(installStatus(check({ admin: "root" }), false)).toEqual({ kind: "ready", lowSpec: false, password: false });
    expect(installStatus(check({ admin: "nopass" }), false)).toEqual({ kind: "ready", lowSpec: false, password: false });
  });

  it("配置低只提醒，照样能装", () => {
    expect(installStatus(check({ lowSpec: true }), false)).toEqual({ kind: "ready", lowSpec: true, password: true });
  });

  it("空间不够时不能装，只列出不够的那些", () => {
    const disks = [
      { mount: "/", available: 30 * 1024 ** 3, need: 1.5 * 1024 ** 3 },
      { mount: "/var", available: 0.5 * 1024 ** 3, need: 1.2 * 1024 ** 3 },
    ];
    expect(installStatus(check({ enoughSpace: false, disks }), false)).toEqual({ kind: "noSpace", disks: [disks[1]] });
  });

  it("没有管理员权限最先说，能退到精简桌面时带上", () => {
    expect(installStatus(check({ admin: "none", enoughSpace: false }), false)).toEqual({ kind: "noAdmin", light: true });
    expect(installStatus(check({ admin: "none", lightAvailable: false }), false)).toEqual({ kind: "noAdmin", light: false });
  });
});
