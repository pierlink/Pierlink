import { describe, expect, it } from "vitest";
import { addFolder, normalizeFolder, pickPowerShell, removeFolder, setServerDir, toggleServer } from "./model";
import type { EntrySettings } from "./api";

const entry: EntrySettings = { enabled: true, approval: "dangerous", windowsCommands: false, folders: [], servers: [], serverDirs: {}, confirmConnections: false };

describe("AI 接入设置", () => {
  it("文件夹去掉引号和末尾斜杠，盘符根目录保留", () => {
    expect(normalizeFolder('  "D:\\work\\"  ')).toBe("D:\\work");
    expect(normalizeFolder("C:")).toBe("C:\\");
    expect(normalizeFolder("C:/")).toBe("C:\\");
    expect(normalizeFolder("   ")).toBe("");
    expect(normalizeFolder("d:/work/demo/")).toBe("d:\\work\\demo");
  });
  it("加文件夹去重（不区分大小写），空的不加", () => {
    const a = addFolder(entry, "D:\\Work");
    expect(a.folders).toEqual(["D:\\Work"]);
    expect(addFolder(a, "d:\\work\\")).toBe(a);
    expect(addFolder(a, " ")).toBe(a);
    expect(addFolder(a, "d:/work")).toBe(a);
    expect(removeFolder(a, "D:\\Work").folders).toEqual([]);
  });
  it("服务器勾选来回切换", () => {
    const a = toggleServer(entry, "s1");
    expect(a.servers).toEqual(["s1"]);
    expect(toggleServer(a, "s1").servers).toEqual([]);
  });
});

describe("服务器的工作目录", () => {
  it("填、改、清掉；没变时原样返回", () => {
    const a = setServerDir(entry, "s1", "  /home/me/project ");
    expect(a.serverDirs).toEqual({ s1: "/home/me/project" });
    expect(setServerDir(a, "s1", "/home/me/project")).toBe(a);
    const b = setServerDir(a, "s2", "~/work");
    expect(b.serverDirs).toEqual({ s1: "/home/me/project", s2: "~/work" });
    expect(setServerDir(b, "s1", "   ").serverDirs).toEqual({ s2: "~/work" });
    expect(setServerDir(entry, "s1", "")).toBe(entry);
    // 旧版本存下的设置里没有这一项
    const old = { ...entry } as Partial<typeof entry>;
    delete old.serverDirs;
    expect(setServerDir(old as typeof entry, "s1", "/srv").serverDirs).toEqual({ s1: "/srv" });
  });
});

describe("AI 在本机用的 shell", () => {
  it("优先 PowerShell 7，其次 Windows PowerShell，都没有时为空", () => {
    const shells = [{ id: "cmd", name: "命令提示符" }, { id: "powershell", name: "Windows PowerShell" }, { id: "pwsh", name: "PowerShell 7" }];
    expect(pickPowerShell(shells)?.id).toBe("pwsh");
    expect(pickPowerShell(shells.slice(0, 2))?.id).toBe("powershell");
    expect(pickPowerShell([{ id: "bash", name: "Git Bash" }])).toBeUndefined();
  });
});
