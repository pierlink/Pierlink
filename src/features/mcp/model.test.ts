import { describe, expect, it } from "vitest";
import { addFolder, normalizeFolder, pickPowerShell, removeFolder, toggleServer } from "./model";
import type { EntrySettings } from "./api";

const entry: EntrySettings = { enabled: true, approval: "dangerous", windowsCommands: false, folders: [], servers: [], remembered: false };

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

describe("AI 在本机用的 shell", () => {
  it("优先 PowerShell 7，其次 Windows PowerShell，都没有时为空", () => {
    const shells = [{ id: "cmd", name: "命令提示符" }, { id: "powershell", name: "Windows PowerShell" }, { id: "pwsh", name: "PowerShell 7" }];
    expect(pickPowerShell(shells)?.id).toBe("pwsh");
    expect(pickPowerShell(shells.slice(0, 2))?.id).toBe("powershell");
    expect(pickPowerShell([{ id: "bash", name: "Git Bash" }])).toBeUndefined();
  });
});
