import { describe, expect, it } from "vitest";
import { nextProgress } from "./installProgress";

describe("装桌面的进度", () => {
  it("更新软件源时的 Get 不算下载的包；汇总行之后才数下载到第几个", () => {
    let p = nextProgress(null, "Hit:1 http://archive.ubuntu.com/ubuntu jammy InRelease\r\nGet:2 http://archive.ubuntu.com/ubuntu jammy-updates InRelease [128 kB]\r\nReading package lists... Done\r\n");
    expect(p.stage).toBe("update");
    expect(p.got).toBe(0);
    p = nextProgress(p, "Reading package lists... Done\r\n0 upgraded, 151 newly installed, 0 to remove and 3 not upgraded.\r\nNeed to get 129 MB of archives.\r\nGet:1 http://archive.ubuntu.com/ubuntu jammy/main amd64 libxfce4util7 [40 kB]\r\nGet:2 http://x y [1 kB]\r\n");
    expect(p.stage).toBe("download");
    expect(p.total).toBe(151);
    expect(p.got).toBe(2);
    // 滚动的末尾一段：重叠的旧行不会把数字往回拉
    p = nextProgress(p, "Get:2 http://x y [1 kB]\r\nGet:37 http://x xfce4-panel [500 kB]\r\n");
    expect(p.got).toBe(37);
    expect(p.line).toContain("xfce4-panel");
  });

  it("dpkg 开始解包、apt 报百分比时进入安装；百分比取最新的", () => {
    let p = nextProgress({ stage: "download", got: 151, total: 151, percent: null, line: "" }, "Selecting previously unselected package xfce4.\r\n");
    expect(p.stage).toBe("install");
    expect(p.percent).toBeNull();
    p = nextProgress(p, "Unpacking xfce4 (4.16) ...\r\n\x1b7\x1b[24;0f\x1b[42m\x1b[30mProgress: [ 37%]\x1b[49m\x1b[39m [##########..........] \x1b8Setting up xfce4 (4.16) ...\r\n\x1b7\x1b[24;0fProgress: [ 41%]\x1b8");
    expect(p.percent).toBe(41);
    expect(p.line).toBe("Setting up xfce4 (4.16) ...");
  });

  it("apt 报错、sudo 拒绝时停在出错，显示报错那行", () => {
    const p = nextProgress(null, "Reading package lists... Done\r\nE: Unable to locate package elementary-xfce-icon-theme\r\nuser@host:~$ ");
    expect(p.stage).toBe("error");
    expect(p.line).toBe("E: Unable to locate package elementary-xfce-icon-theme");
    const s = nextProgress(null, "[sudo] password for pl: \r\nsudo: 3 incorrect password attempts\r\n");
    expect(s.stage).toBe("error");
    expect(s.line).toContain("incorrect password attempts");
  });

  it("阶段只往前走", () => {
    const p = nextProgress({ stage: "install", got: 10, total: 10, percent: 80, line: "" }, "Get:3 http://x y\r\nReading package lists...\r\n");
    expect(p.stage).toBe("install");
    expect(p.percent).toBe(80);
  });
});
