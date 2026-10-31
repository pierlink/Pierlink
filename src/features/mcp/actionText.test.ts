import { describe, expect, it } from "vitest";
import { localizeAction } from "./actionText";

describe("确认框操作说明的英文显示", () => {
  it("中文界面原样显示", () => {
    const text = "删除路径：D:\\work\\tmp\n递归删除：true";
    expect(localizeAction(text, "zh-CN")).toBe(text);
  });

  it("删除、重命名、建目录的标题和标注换成英文，路径不动", () => {
    expect(localizeAction("删除路径：D:\\工作\\旧\n递归删除：true", "en")).toBe("Delete: D:\\工作\\旧\nRecursive: yes");
    expect(localizeAction("重命名：D:\\a.txt → D:\\b.txt\n覆盖已有目标：false（目录连同内容移动，不递归删除）", "en"))
      .toBe("Rename: D:\\a.txt → D:\\b.txt\nOverwrite an existing target: no (a folder moves with its contents)");
    expect(localizeAction("创建目录（含父目录）：D:\\x\\y", "en")).toBe("Create folder (and missing parents): D:\\x\\y");
  });

  it("改文件的旧文字、新文字和截断标记；内容里同样的字不动", () => {
    const text = "写入脚本、程序或快捷方式可能执行命令：D:\\run.ps1\n旧文字：\nWrite-Output 旧文字：\n新文字：\nabc…（共 900 字）";
    expect(localizeAction(text, "en")).toBe(
      "Writing scripts, programs or shortcuts can run commands: D:\\run.ps1\nOld text:\nWrite-Output 旧文字：\nNew text:\nabc… (900 characters in total)",
    );
    expect(localizeAction("写入文件：D:\\a.md\n内容：12 字节，前 6 行：\n# 标题", "en")).toBe("Write file: D:\\a.md\nContent: 12 bytes, first 6 lines:\n# 标题");
  });

  it("命令的工作目录和终端里内容未知的提示", () => {
    expect(localizeAction("Remove-Item -Recurse x\n（在目录 D:\\work 里执行）", "en")).toBe("Remove-Item -Recurse x\n(runs in D:\\work)");
    expect(localizeAction("（翻历史后回车，内容未知）", "en")).toBe("(Enter after recalling history: the command is unknown)");
    expect(localizeAction("（终端里可能有交给 AI 之前或你刚打的、还没回车的文字，AI 按回车，实际执行的内容未知）\nrm -rf x", "en"))
      .toMatch(/^\(The terminal may hold text .*\)\nrm -rf x$/);
  });

  it("普通命令原样显示", () => {
    expect(localizeAction("git clean -fd", "en")).toBe("git clean -fd");
  });
});
