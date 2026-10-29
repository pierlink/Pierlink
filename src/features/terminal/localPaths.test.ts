import { describe, expect, it } from "vitest";
import { droppedText, explorerFolder, shellPath, shellStyle } from "./localPaths";

const PWSH = "c:\\program files\\powershell\\7\\pwsh.exe";
const WINPS = "c:\\windows\\system32\\windowspowershell\\v1.0\\powershell.exe";
const CMD = "c:\\windows\\system32\\cmd.exe";
const WSL = "c:\\windows\\system32\\wsl.exe#Ubuntu";
const GITBASH = "c:\\program files\\git\\bin\\bash.exe";

describe("本机终端种类", () => {
  it("按程序路径和 WSL 发行版区分", () => {
    expect(shellStyle(PWSH)).toEqual({ kind: "powershell" });
    expect(shellStyle(WINPS)).toEqual({ kind: "powershell" });
    expect(shellStyle(CMD)).toEqual({ kind: "cmd" });
    expect(shellStyle(WSL)).toEqual({ kind: "wsl", distro: "Ubuntu" });
    expect(shellStyle("c:\\windows\\system32\\wsl.exe#Debian 中文")).toEqual({ kind: "wsl", distro: "Debian 中文" });
    expect(shellStyle(GITBASH)).toEqual({ kind: "msys" });
    expect(shellStyle("c:\\tools\\nu.exe")).toEqual({ kind: "other" });
  });
});

describe("拖进终端的路径", () => {
  it("PowerShell：普通路径原样，有空格或特殊字符加单引号，单引号写两个", () => {
    const s = shellStyle(PWSH);
    expect(shellPath(s, "C:\\Users\\me\\a.txt")).toBe("C:\\Users\\me\\a.txt");
    expect(shellPath(s, "C:\\Users\\me\\a b.txt")).toBe("'C:\\Users\\me\\a b.txt'");
    expect(shellPath(s, "C:\\it's\\x")).toBe("'C:\\it''s\\x'");
    expect(shellPath(s, "C:\\a&b(1)\\$x")).toBe("'C:\\a&b(1)\\$x'");
  });

  it("命令提示符：有空格或特殊字符加双引号", () => {
    const s = shellStyle(CMD);
    expect(shellPath(s, "C:\\ab\\c.txt")).toBe("C:\\ab\\c.txt");
    expect(shellPath(s, "C:\\a b\\c.txt")).toBe("\"C:\\a b\\c.txt\"");
    expect(shellPath(s, "C:\\a&b")).toBe("\"C:\\a&b\"");
  });

  it("WSL：盘符路径换成 /mnt/盘符，本发行版的共享路径换成 Linux 路径", () => {
    const s = shellStyle(WSL);
    expect(shellPath(s, "C:\\Users\\me\\a b.txt")).toBe("'/mnt/c/Users/me/a b.txt'");
    expect(shellPath(s, "D:\\x")).toBe("/mnt/d/x");
    expect(shellPath(s, "C:\\")).toBe("/mnt/c");
    expect(shellPath(s, "C:\\it's")).toBe("'/mnt/c/it'\\''s'");
    expect(shellPath(s, "\\\\wsl.localhost\\Ubuntu\\home\\me\\f.txt")).toBe("/home/me/f.txt");
    expect(shellPath(s, "\\\\wsl$\\ubuntu\\home\\me")).toBe("/home/me");
    // 别的发行版、网络共享：没法换成这个发行版里的路径，原样加引号
    expect(shellPath(s, "\\\\wsl.localhost\\Debian\\x")).toBe("'\\\\wsl.localhost\\Debian\\x'");
  });

  it("Git Bash：盘符路径换成 /盘符", () => {
    const s = shellStyle(GITBASH);
    expect(shellPath(s, "C:\\Users\\me\\x.txt")).toBe("/c/Users/me/x.txt");
    expect(shellPath(s, "E:\\a b")).toBe("'/e/a b'");
  });

  it("多个文件用空格隔开，不带回车", () => {
    expect(droppedText(PWSH, ["C:\\a.txt", "C:\\b c.txt"])).toBe("C:\\a.txt 'C:\\b c.txt'");
    expect(droppedText(WSL, ["C:\\a.txt"])).toBe("/mnt/c/a.txt");
  });
});

describe("在资源管理器中打开的文件夹", () => {
  it("PowerShell、命令提示符报告的是 Windows 路径，原样使用", () => {
    expect(explorerFolder(PWSH, "C:\\Users\\me")).toBe("C:\\Users\\me");
    expect(explorerFolder(CMD, "C:/Users/me")).toBe("C:\\Users\\me");
    expect(explorerFolder(PWSH, "\\\\server\\share\\x")).toBe("\\\\server\\share\\x");
  });

  it("WSL：/mnt/盘符 换回盘符，其余走 \\\\wsl.localhost", () => {
    expect(explorerFolder(WSL, "/home/me")).toBe("\\\\wsl.localhost\\Ubuntu\\home\\me");
    expect(explorerFolder(WSL, "/mnt/c/Users")).toBe("C:\\Users");
    expect(explorerFolder(WSL, "/mnt/d")).toBe("D:\\");
  });

  it("Git Bash：/盘符 换回盘符，其余对应不上", () => {
    expect(explorerFolder(GITBASH, "/c/Users")).toBe("C:\\Users");
    expect(explorerFolder(GITBASH, "/home/me")).toBeNull();
  });

  it("不知道当前目录或是相对路径时没有", () => {
    expect(explorerFolder(PWSH, null)).toBeNull();
    expect(explorerFolder(PWSH, "relative\\dir")).toBeNull();
  });
});
