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

  it("传文件的标题和大小；写到要确认的位置时原因在第一行", () => {
    expect(localizeAction("传输文件：Windows D:\\a.zip → 计算服务器 /home/me/a.zip\n大小：2048 字节；覆盖已有目标：false", "en"))
      .toBe("Transfer file: Windows D:\\a.zip → 计算服务器 /home/me/a.zip\nSize: 2048 bytes; overwrite an existing target: no");
    expect(localizeAction("写入 SSH、Git 或 shell 的配置可能让它们执行命令：/home/me/.bashrc\n传输文件：Windows D:\\rc → 计算服务器 /home/me/.bashrc\n大小：10 字节；覆盖已有目标：true", "en"))
      .toBe("Writing SSH, Git or shell configuration can make them run commands: /home/me/.bashrc\nTransfer file: Windows D:\\rc → 计算服务器 /home/me/.bashrc\nSize: 10 bytes; overwrite an existing target: yes");
  });

  it("用默认程序打开文件和网址", () => {
    expect(localizeAction("用默认程序打开：D:\\工作\\报告.pdf", "en")).toBe("Open with the default app: D:\\工作\\报告.pdf");
    expect(localizeAction("用默认浏览器打开网址：https://example.com/用默认程序打开：", "en")).toBe("Open in the default browser: https://example.com/用默认程序打开：");
    expect(localizeAction("把图片传到服务器的临时目录，并把路径粘贴进终端：D:\\work\\shot.png", "en")).toBe("Upload an image to the server's temp folder and paste its path into the terminal: D:\\work\\shot.png");
  });

  it("复制的标题、数量和要确认的文件", () => {
    const text = "复制：D:\\工作\\src → D:\\工作\\dst\n文件 3 个、目录 2 个，共 1024 字节；覆盖已有目标：true\n会写入要确认的文件（共 1 项）：\nD:\\工作\\dst\\run.ps1";
    expect(localizeAction(text, "en")).toBe(
      "Copy: D:\\工作\\src → D:\\工作\\dst\n3 files and 2 folders, 1024 bytes; overwrite existing targets: yes\nWrites files that need confirmation (1 in total):\nD:\\工作\\dst\\run.ps1",
    );
  });

  it("改文件的旧文字、新文字和截断标记；内容里同样的字不动", () => {
    const text = "写入脚本、程序或快捷方式可能执行命令：D:\\run.ps1\n旧文字：\nWrite-Output 旧文字：\n新文字：\nabc…（共 900 字）";
    expect(localizeAction(text, "en")).toBe(
      "Writing scripts, programs or shortcuts can run commands: D:\\run.ps1\nOld text:\nWrite-Output 旧文字：\nNew text:\nabc… (900 characters in total)",
    );
    expect(localizeAction("写入文件：D:\\a.md\n内容：12 字节，前 6 行：\n# 标题", "en")).toBe("Write file: D:\\a.md\nContent: 12 bytes, first 6 lines:\n# 标题");
    expect(localizeAction("写入脚本、程序或快捷方式可能执行命令：D:\\bin\\tool.exe\n内容：6 字节（content_base64，原样写入）", "en"))
      .toBe("Writing scripts, programs or shortcuts can run commands: D:\\bin\\tool.exe\nContent: 6 bytes (content_base64, written as-is)");
  });

  it("多处修改的数量、每处的旧文字新文字和没列出的处数", () => {
    const text = "写入文件：D:\\a.ts\n共 7 处修改\n第 1 处旧文字：\nfoo\n第 1 处新文字：\nbar\n第 2 处旧文字：\nx\n第 2 处新文字：\ny\n…（还有 5 处）";
    expect(localizeAction(text, "en")).toBe(
      "Write file: D:\\a.ts\n7 edits\nEdit 1, old text:\nfoo\nEdit 1, new text:\nbar\nEdit 2, old text:\nx\nEdit 2, new text:\ny\n… (5 more)",
    );
  });

  it("命令的工作目录和终端里内容未知的提示", () => {
    expect(localizeAction("Remove-Item -Recurse x\n（在目录 D:\\work 里执行）", "en")).toBe("Remove-Item -Recurse x\n(runs in D:\\work)");
    expect(localizeAction("（翻历史后回车，内容未知）", "en")).toBe("(Enter after recalling history: the command is unknown)");
    expect(localizeAction("（终端里可能有交给 AI 之前或你刚打的、还没回车的文字，AI 按回车，实际执行的内容未知）\nrm -rf x", "en"))
      .toMatch(/^\(The terminal may hold text .*\)\nrm -rf x$/);
    expect(localizeAction("（AI 撤销了修改或粘回了删掉的文字，按回车实际执行的内容未知）\nls", "en"))
      .toBe("(AI undid an edit or pasted back deleted text, so what runs on Enter is unknown)\nls");
  });

  it("普通命令原样显示", () => {
    expect(localizeAction("git clean -fd", "en")).toBe("git clean -fd");
  });
});
