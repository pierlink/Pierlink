import { describe, expect, it } from "vitest";
import { KittyKeyboard, localShellFamily, newlineKey, parseOsc52 } from "./kittyKeys";

const key = (k: Partial<KeyboardEvent> & { key: string }) => ({ shiftKey: false, altKey: false, ctrlKey: false, metaKey: false, ...k });

describe("kitty 键盘协议", () => {
  it("程序没开启时 Shift+Enter 照旧", () => {
    const kb = new KittyKeyboard();
    expect(kb.encode(key({ key: "Enter", shiftKey: true }))).toBeNull();
    expect(kb.query()).toBe("\x1b[?0u");
  });

  it("开启后 Shift+Enter、Ctrl+Enter 按协议编码，普通 Enter 不变", () => {
    const kb = new KittyKeyboard();
    kb.push(1);
    expect(kb.query()).toBe("\x1b[?1u");
    expect(kb.encode(key({ key: "Enter", shiftKey: true }))).toBe("\x1b[13;2u");
    expect(kb.encode(key({ key: "Enter", ctrlKey: true }))).toBe("\x1b[13;5u");
    expect(kb.encode(key({ key: "Enter" }))).toBeNull();
    expect(kb.encode(key({ key: "a", shiftKey: true }))).toBeNull();
  });

  it("只记支持的标志；弹栈回到原来的状态", () => {
    const kb = new KittyKeyboard();
    kb.push(31);
    expect(kb.flags).toBe(1);
    kb.pop(1);
    expect(kb.flags).toBe(0);
    kb.pop(3);
    expect(kb.flags).toBe(0);
  });

  it("设置的三种方式", () => {
    const kb = new KittyKeyboard();
    kb.set(1, 1);
    expect(kb.flags).toBe(1);
    kb.set(1, 3);
    expect(kb.flags).toBe(0);
    kb.set(1, 2);
    expect(kb.flags).toBe(1);
  });

  it("全屏程序在备用屏开启，回到主屏后 shell 不受影响", () => {
    const kb = new KittyKeyboard();
    kb.setScreen(true);
    kb.push(1);
    kb.setScreen(false);
    expect(kb.encode(key({ key: "Enter", shiftKey: true }))).toBeNull();
    kb.setScreen(true);
    expect(kb.encode(key({ key: "Enter", shiftKey: true }))).toBe("\x1b[13;2u");
  });
});

describe("Shift+Enter、Ctrl+Enter：和常用终端一样是换行，原生功能不变", () => {
  const shift = key({ key: "Enter", shiftKey: true });
  const ctrl = key({ key: "Enter", ctrlKey: true });
  const at = (local: "powershell" | "cmd" | "other" | null, altScreen = false, kitty = false) => ({ kitty, local, altScreen });
  it("服务器上和本机 Git Bash、WSL 的 shell：插入换行不执行", () => {
    for (const ev of [shift, ctrl]) {
      expect(newlineKey(ev, at(null))).toBe("\x16\n");
      expect(newlineKey(ev, at("other"))).toBe("\x16\n");
    }
  });
  it("程序开启了 kitty 协议：都按 Shift+Enter 发", () => {
    expect(newlineKey(shift, at(null, false, true))).toBe("\x1b[13;2u");
    expect(newlineKey(ctrl, at(null, true, true))).toBe("\x1b[13;2u");
  });
  it("全屏程序（含 screen、tmux 里的 Claude Code）发换行符", () => {
    expect(newlineKey(shift, at(null, true))).toBe("\n");
    expect(newlineKey(ctrl, at("powershell", true))).toBe("\n");
  });
  it("本机 PowerShell、命令提示符：三个键都把真实按键交给 Windows 控制台", () => {
    for (const shell of ["powershell", "cmd"] as const) {
      expect(newlineKey(shift, at(shell))).toBe("\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_");
      expect(newlineKey(ctrl, at(shell))).toBe("\x1b[13;28;10;1;8;1_\x1b[13;28;10;0;8;1_");
      expect(newlineKey(key({ key: "j", ctrlKey: true }), at(shell))).toBe("\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_");
    }
  });
  it("别的键和组合不管（不在本机控制台时的 Ctrl+J、回车、Ctrl+Shift+Enter、Alt+Enter）", () => {
    expect(newlineKey(key({ key: "Enter" }), at(null))).toBeNull();
    expect(newlineKey(key({ key: "j", ctrlKey: true }), at(null))).toBeNull();
    expect(newlineKey(key({ key: "j", ctrlKey: true }), at("other"))).toBeNull();
    expect(newlineKey(key({ key: "Enter", shiftKey: true, ctrlKey: true }), at(null))).toBeNull();
    expect(newlineKey(key({ key: "Enter", altKey: true }), at(null))).toBeNull();
  });
  it("按程序路径认出本机 shell", () => {
    expect(localShellFamily("c:\\program files\\powershell\\7\\pwsh.exe")).toBe("powershell");
    expect(localShellFamily("c:\\windows\\system32\\windowspowershell\\v1.0\\powershell.exe")).toBe("powershell");
    expect(localShellFamily("c:\\windows\\system32\\cmd.exe")).toBe("cmd");
    expect(localShellFamily("c:\\program files\\git\\bin\\bash.exe")).toBe("other");
    expect(localShellFamily("c:\\windows\\system32\\wsl.exe#ubuntu")).toBe("other");
  });
});

describe("OSC 52 剪贴板", () => {
  it("写入：解出 UTF-8 文字", () => {
    const b64 = btoa(String.fromCharCode(...new TextEncoder().encode("你好 copy")));
    expect(parseOsc52(`c;${b64}`)).toBe("你好 copy");
    expect(parseOsc52(`;${b64}`)).toBe("你好 copy");
  });

  it("读请求、空内容、格式不对、太长都忽略", () => {
    expect(parseOsc52("c;?")).toBeNull();
    expect(parseOsc52("c;")).toBeNull();
    expect(parseOsc52("nosemicolon")).toBeNull();
    expect(parseOsc52("c;@@@")).toBeNull();
    expect(parseOsc52(`c;${"A".repeat(100)}`, 30)).toBeNull();
  });
});
