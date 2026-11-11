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
    let p = nextProgress({ stage: "download", waiting: null, got: 151, total: 151, percent: null, line: "" }, "Selecting previously unselected package xfce4.\r\n");
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

  it("登录的欢迎语、提示符不算开始更新软件源；等 sudo 密码、apt 被占着时照实说", () => {
    let p = nextProgress(null, "Welcome to Ubuntu 22.04.4 LTS\r\n * Documentation:  https://help.ubuntu.com\r\nubuntu@vm:~$ ");
    expect(p.stage).toBe("start");
    expect(p.waiting).toBeNull();
    p = nextProgress(p, "ubuntu@vm:~$ sudo apt-get update && sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y xfce4\r\n[sudo] password for ubuntu: ");
    expect(p.stage).toBe("start");
    expect(p.waiting).toBe("password");
    // 中文系统的提示
    expect(nextProgress(null, "[sudo] ubuntu 的密码：").waiting).toBe("password");
    // 输对密码、apt 开始了
    p = nextProgress(p, "[sudo] password for ubuntu: \r\nHit:1 http://archive.ubuntu.com/ubuntu jammy InRelease\r\n");
    expect(p.stage).toBe("update");
    expect(p.waiting).toBeNull();
    p = nextProgress(p, "Reading package lists... Done\r\nWaiting for cache lock: Could not get lock /var/lib/dpkg/lock-frontend. It is held by process 2468 (unattended-upgr)... 12s");
    expect(p.waiting).toBe("lock");
    p = nextProgress(p, "0 upgraded, 151 newly installed, 0 to remove and 3 not upgraded.\r\nGet:1 http://x y [1 kB]\r\n");
    expect(p.stage).toBe("download");
    expect(p.waiting).toBeNull();
    // 输错三次：出错，不再说在等密码
    const wrong = nextProgress({ ...p, stage: "start", waiting: "password" }, "Sorry, try again.\r\nsudo: 3 incorrect password attempts\r\n");
    expect(wrong.stage).toBe("error");
    expect(wrong.waiting).toBeNull();
  });

  it("彩色的提示符照样是一整行", () => {
    const p = nextProgress(null, "\x1b[01;32mubuntu@VM-0-3-ubuntu\x1b[00m:\x1b[01;34m~\x1b[00m$ ");
    expect(p.line).toBe("ubuntu@VM-0-3-ubuntu:~$");
  });

  it("命令没有自动输入的说明一直保留到 apt 开始", () => {
    let p = nextProgress({ stage: "start", waiting: "notTyped", got: 0, total: null, percent: null, line: "" }, "\x1b[?1049h\x1b[H screen 里的程序在刷新\r\n");
    expect(p.waiting).toBe("notTyped");
    p = nextProgress(p, "Hit:1 http://archive.ubuntu.com/ubuntu jammy InRelease\r\n");
    expect(p.stage).toBe("update");
    expect(p.waiting).toBeNull();
  });

  it("阶段只往前走", () => {
    const p = nextProgress({ stage: "install", waiting: null, got: 10, total: 10, percent: 80, line: "" }, "Get:3 http://x y\r\nReading package lists...\r\n");
    expect(p.stage).toBe("install");
    expect(p.percent).toBe(80);
  });
});
