import { describe, expect, it } from "vitest";
import { keepsConnectCard, looksTyped } from "./loginOutput";

describe("连接卡片盖着时的输出", () => {
  it("只有控制序列：继续盖着", () => {
    expect(keepsConnectCard("\x1b]0;ssh.exe\x07\x1b[?25l\x1b[2J\x1b[H")).toBe(true);
    // 键盘应用模式、字符集切换这类 ESC 加一两个字符的序列
    expect(keepsConnectCard("\x1b=\x1b>\x1b(B\x1b)0\x1b7\x1b8")).toBe(true);
    expect(keepsConnectCard("\r\n")).toBe(true);
  });
  it("密码不对、登录被拒：继续盖着（后面就是失败的提示块）", () => {
    expect(keepsConnectCard("Permission denied, please try again.\r\n")).toBe(true);
    expect(keepsConnectCard("pltest@127.0.0.1: Permission denied (publickey,password).\r\n")).toBe(true);
    expect(keepsConnectCard("Received disconnect from 10.0.0.2 port 22:2: Too many authentication failures\r\nDisconnected from 10.0.0.2 port 22\r\n")).toBe(true);
  });
  it("密码提示、警告、欢迎信息：让开", () => {
    expect(keepsConnectCard("Permission denied, please try again.\r\npltest@h's password: ")).toBe(false);
    expect(keepsConnectCard("Warning: Permanently added '10.0.0.2' (ED25519) to the list of known hosts.\r\n")).toBe(false);
    expect(keepsConnectCard("Welcome to Ubuntu 22.04.4 LTS\r\n")).toBe(false);
    expect(keepsConnectCard("\x1b[1;32mdev@box\x1b[0m:~$ ")).toBe(false);
  });
});

describe("像不像打字（广播只发打字的）", () => {
  it("文字、控制键、方向键、编辑键、Alt+键、粘贴算", () => {
    for (const d of ["ls -l", "\r", "\x03", "\x1b", "\x1b[A", "\x1b[1;5C", "\x1b[Z", "\x1b[3~", "\x1b[13;2u", "\x1bOA", "\x1bOP", "\x1bb", "\x1b[200~a\rb\x1b[201~", "\x1b[1;2P", "\x1b[1;5S", "\x1bOp", "\x1bOy", "\x1bOM"]) {
      expect(looksTyped(d), JSON.stringify(d)).toBe(true);
    }
  });
  it("焦点进出、设备属性、光标位置、颜色应答、鼠标报告不算", () => {
    for (const d of ["\x1b[I", "\x1b[O", "\x1b[?62;22c", "\x1b[>0;276;0c", "\x1b[12;40R", "\x1b[1;5R", "\x1b]11;rgb:1010/1010/1010\x1b\\", "\x1bP1$r0m\x1b\\", "\x1b[<0;10;5M", "\x1b[<0;10;5m", "\x1b[?2004;1$y"]) {
      expect(looksTyped(d), JSON.stringify(d)).toBe(false);
    }
  });
});
