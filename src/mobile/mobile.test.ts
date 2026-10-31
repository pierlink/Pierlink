import { describe, expect, it } from "vitest";
import { color256, parseSgrLine, stripSgr } from "./ansi";
import { detectChoices } from "./choices";

describe("手机屏幕的颜色", () => {
  it("没有颜色码时整行一段、样式为空", () => {
    expect(parseSgrLine("plain text")).toEqual([{ text: "plain text", style: {} }]);
  });

  it("基本色、亮色、粗体和复位", () => {
    const segs = parseSgrLine("\x1b[1;31mERR\x1b[0m ok \x1b[92mgreen\x1b[39m!");
    expect(segs).toEqual([
      { text: "ERR", style: { bold: true, fg: "var(--ansi-1)" } },
      { text: " ok ", style: {} },
      { text: "green", style: { fg: "var(--ansi-10)" } },
      { text: "!", style: {} },
    ]);
  });

  it("256 色和真彩色，前景背景都行", () => {
    expect(parseSgrLine("\x1b[38;5;196mA")[0].style.fg).toBe(color256(196));
    expect(parseSgrLine("\x1b[48;2;10;20;300mB")[0].style.bg).toBe("rgb(10, 20, 255)");
    expect(color256(16)).toBe("rgb(0, 0, 0)");
    expect(color256(231)).toBe("rgb(255, 255, 255)");
    expect(color256(232)).toBe("rgb(8, 8, 8)");
    expect(color256(3)).toBe("var(--ansi-3)");
  });

  it("反色、暗、斜体、下划线可以单独关掉", () => {
    const segs = parseSgrLine("\x1b[2;3;4;7mx\x1b[22;23;24;27my");
    expect(segs[0].style).toEqual({ dim: true, italic: true, underline: true, inverse: true });
    expect(segs[1].style).toEqual({});
  });

  it("相同样式的相邻段合并；去掉其他控制字符", () => {
    expect(parseSgrLine("\x1b[31ma\x1b[31mb\x07c")).toEqual([{ text: "abc", style: { fg: "var(--ansi-1)" } }]);
    expect(stripSgr("\x1b[1;32mok\x1b[0m done")).toBe("ok done");
  });
});

describe("认出等着回答的选项", () => {
  it("Claude Code 的编号菜单", () => {
    const screen = [
      "Bash command",
      "  rm -rf build/",
      "Do you want to proceed?",
      "❯ 1. Yes",
      "  2. Yes, and don't ask again for rm commands in /home/dev/web",
      "  3. No, and tell Claude what to do differently (esc)",
      "",
    ];
    expect(detectChoices(screen)).toEqual([
      { key: "1", label: "Yes", current: true },
      { key: "2", label: "Yes, and don't ask again for rm commands in /home/dev/web" },
      { key: "3", label: "No, and tell Claude what to do differently (esc)" },
    ]);
  });

  it("带边框的菜单、选项之间夹着说明行", () => {
    const screen = [
      "│ Select model                         │",
      "│ ❯ 1. Default (recommended)           │",
      "│      Opus for complex tasks          │",
      "│   2. Sonnet                          │",
      "╰──────────────────────────────────────╯",
    ];
    const got = detectChoices(screen);
    expect(got?.map((c) => c.key)).toEqual(["1", "2"]);
    expect(got?.[0].current).toBe(true);
  });

  it("(y/n) 和 [Y/n] 提示", () => {
    expect(detectChoices(["Do you want to continue? [Y/n] "])).toEqual([{ key: "y", label: "y", enter: true }, { key: "n", label: "n", enter: true }]);
    expect(detectChoices(["Overwrite file? (y/n)"])?.length).toBe(2);
  });

  it("普通输出、编号列表在很靠上的位置都不算", () => {
    expect(detectChoices(["dev@gpu:~$ ls", "a.txt b.txt", "dev@gpu:~$ "])).toBeNull();
    const old = ["1. first", "2. second", ...Array.from({ length: 20 }, (_, i) => `line ${i}`)];
    expect(detectChoices(old)).toBeNull();
    // 只有一项不算菜单
    expect(detectChoices(["❯ 1. Yes"])).toBeNull();
  });

  it("Claude 回答末尾的编号列表（后面是输入框和状态栏）不算；紧挨着最底下的不带标记的编号算（bash 的 select）", () => {
    const answer = [
      "⏺ 可以这样做：",
      "  1. 先备份数据库",
      "  2. 再跑迁移",
      "  3. 最后重启服务",
      "╭──────────────────────────────╮",
      "│ >                            │",
      "╰──────────────────────────────╯",
      "  ? for shortcuts",
    ];
    expect(detectChoices(answer)).toBeNull();
    expect(detectChoices(["1) build", "2) test", "#? "])?.map((c) => c.key)).toEqual(["1", "2"]);
  });

  it("选项太长折行时仍然连成一组", () => {
    const screen = [
      " Do you want to proceed?",
      " ❯ 1. Yes",
      "   2. Yes, and don't ask again for",
      "      npm commands in /home/dev/web",
      "   3. No (esc)",
    ];
    expect(detectChoices(screen)?.map((c) => c.key)).toEqual(["1", "2", "3"]);
  });
});
