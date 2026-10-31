import { describe, expect, it } from "vitest";
import { parse133, parseMark, ShellTracker, type TermLike } from "./shellMarks";

/** 只放 ASCII 的假终端：每行一段文字，可以标成折行续行 */
function fakeTerm(cols = 40) {
  const lines: { text: string; isWrapped: boolean }[] = [];
  const t = {
    cols,
    type: "normal" as "normal" | "alternate",
    cursorX: 0,
    cursorY: 0,
    baseY: 0,
    lines,
    /** 把第 y 行写成 text，光标放到这一行的 x 列 */
    put(y: number, text: string, x = text.length, isWrapped = false) {
      while (lines.length <= y) lines.push({ text: "", isWrapped: false });
      lines[y] = { text, isWrapped };
      t.cursorY = y - t.baseY;
      t.cursorX = x;
    },
  };
  const term: TermLike = {
    cols,
    buffer: {
      get active() {
        return {
          type: t.type,
          cursorX: t.cursorX,
          cursorY: t.cursorY,
          baseY: t.baseY,
          getLine: (y: number) => {
            const l = lines[y];
            return l && { isWrapped: l.isWrapped, translateToString: (trim = false, start = 0, end = cols) => { const s = l.text.padEnd(cols).slice(start, end); return trim ? s.trimEnd() : s; } };
          },
        };
      },
    },
    registerMarker: () => {
      const m = { line: t.baseY + t.cursorY, isDisposed: false, dispose() { m.isDisposed = true; } };
      return m;
    },
  };
  return { t, term };
}

describe("提示符标记", () => {
  it("解析 OSC 133 和私有标记", () => {
    expect(parse133("A")).toEqual({ kind: "promptStart" });
    expect(parse133("A;cl=m")).toEqual({ kind: "promptStart" });
    expect(parse133("B")).toEqual({ kind: "inputStart" });
    expect(parse133("D;1")).toEqual({ kind: "commandEnd", exit: 1 });
    expect(parse133("D")).toEqual({ kind: "commandEnd", exit: null });
    expect(parse133("C")).toBeNull();
    expect(parse133("A;k=s")).toEqual({ kind: "promptStart", secondary: true });
    expect(parse133("A;cl=m;k=c")).toEqual({ kind: "promptStart", secondary: true });
    expect(parse133("A;k=i")).toEqual({ kind: "promptStart" });
    expect(parse133("A;k=r")).toBeNull();
    expect(parseMark("i;native=1")).toEqual({ kind: "init", native: true });
    expect(parseMark("i;native=0")).toEqual({ kind: "init", native: false });
    const b64 = btoa(String.fromCharCode(...new TextEncoder().encode("echo 中文")));
    expect(parseMark(`c;0;${b64}`)).toEqual({ kind: "command", exit: 0, command: "echo 中文" });
    expect(parseMark("c;0;不是base64")).toBeNull();
    expect(parseMark("c;0")).toBeNull();
    expect(parseMark("x;1")).toBeNull();
  });

  it("B 之后取到输入；回车后不再有输入", () => {
    const { t, term } = fakeTerm();
    const s = new ShellTracker();
    t.put(0, "user@h:~$ ");
    s.onMark({ kind: "inputStart" }, term, "/home/u");
    expect(s.integrated).toBe(true);
    t.put(0, "user@h:~$ git st");
    expect(s.input(term)).toEqual({ text: "git st", room: 40 - 16 });
    s.leave();
    expect(s.input(term)).toBeNull();
  });

  it("在提示符下回车后算命令在运行，回到提示符为止；不在提示符下回车不算", () => {
    const { t, term } = fakeTerm();
    const s = new ShellTracker();
    s.enter();
    expect(s.running).toBe(false);
    t.put(0, "user@h:~$ ");
    s.onMark({ kind: "inputStart" }, term, "/home/u");
    t.put(0, "user@h:~$ claude");
    s.enter();
    expect(s.running).toBe(true);
    expect(s.input(term)).toBeNull();
    s.onMark({ kind: "commandEnd", exit: 0 }, term, "/home/u");
    expect(s.running).toBe(false);
    s.onMark({ kind: "inputStart" }, term, "/home/u");
    s.enter();
    expect(s.running).toBe(true);
    s.restart();
    expect(s.running).toBe(false);
  });

  it("续行提示符（PS2）后面的 B 不取输入，下一个主提示符照常", () => {
    const { t, term } = fakeTerm();
    const s = new ShellTracker();
    t.put(0, "> ");
    s.onMark({ kind: "promptStart", secondary: true }, term, "/home/u");
    s.onMark({ kind: "inputStart" }, term, "/home/u");
    t.put(0, "> echo");
    expect(s.input(term)).toBeNull();
    s.onMark({ kind: "commandEnd", exit: 0 }, term, "/home/u");
    s.onMark({ kind: "promptStart" }, term, "/home/u");
    t.put(0, "$ ");
    s.onMark({ kind: "inputStart" }, term, "/home/u");
    t.put(0, "$ git");
    expect(s.input(term)).toEqual({ text: "git", room: 40 - 5 });
  });

  it("命令记录属于收到时的目录", () => {
    const { term } = fakeTerm();
    const s = new ShellTracker();
    const h = s.onMark({ kind: "command", exit: 0, command: "cd /tmp" }, term, "/home/u");
    expect(h).toMatchObject({ command: "cd /tmp", cwd: "/home/u", exit: 0 });
  });

  it("提示符被 Ctrl+R 的搜索提示盖掉、换到全屏程序时不给输入", () => {
    const { t, term } = fakeTerm();
    const s = new ShellTracker();
    t.put(0, "$ ");
    s.onMark({ kind: "inputStart" }, term, null);
    t.put(0, "(reverse-i-search)`': ");
    expect(s.input(term)).toBeNull();
    t.put(0, "$ ls");
    t.type = "alternate";
    expect(s.input(term)).toBeNull();
    t.type = "normal";
    expect(s.input(term)?.text).toBe("ls");
  });

  it("长命令折到下一行时拼起来；输入没到头时不给", () => {
    const { t, term } = fakeTerm(10);
    const s = new ShellTracker();
    t.put(0, "$ ");
    s.onMark({ kind: "inputStart" }, term, null);
    t.put(0, "$ echo abc", 10);
    t.put(1, "defg", 4, true);
    expect(s.input(term)?.text).toBe("echo abcdefg");
    // 光标回到第一行：后面还有折行续行
    t.cursorY = 0;
    t.cursorX = 8;
    expect(s.input(term)).toBeNull();
    // 下一行不是折行续行（是程序输出）：光标不在输入那一段上
    const { t: t2, term: term2 } = fakeTerm(10);
    const s2 = new ShellTracker();
    t2.put(0, "$ ");
    s2.onMark({ kind: "inputStart" }, term2, null);
    t2.put(1, "other", 5, false);
    expect(s2.input(term2)).toBeNull();
  });

  it("光标在输入中间时不给；右侧提示符不算", () => {
    const { t, term } = fakeTerm(30);
    const s = new ShellTracker();
    t.put(0, "% ");
    s.onMark({ kind: "inputStart" }, term, null);
    // 光标在 echo 后面，后面还有 hello
    t.put(0, "% echo hello", 6);
    expect(s.input(term)).toBeNull();
    // 光标后面隔一格就是字
    t.put(0, "% echo  hello", 7);
    expect(s.input(term)).toBeNull();
    // zsh 右侧提示符靠右（离行尾一格）：照常给，空位到右侧提示符为止
    t.put(0, "% git".padEnd(22) + "main 10:30", 5);
    expect(s.input(term)).toEqual({ text: "git", room: 17 });
  });

  it("A、D 和重连都让输入失效；shell 自带预测时标出来", () => {
    const { t, term } = fakeTerm();
    const s = new ShellTracker();
    t.put(0, "$ ");
    s.onMark({ kind: "inputStart" }, term, null);
    t.put(0, "$ x");
    s.onMark({ kind: "promptStart" }, term, null);
    expect(s.input(term)).toBeNull();
    s.onMark({ kind: "init", native: true }, term, null);
    expect(s.native).toBe(true);
  });

  it("重新连接后钩子和自带预测都要等新会话的标记", () => {
    const { t, term } = fakeTerm();
    const s = new ShellTracker();
    t.put(0, "$ ");
    s.onMark({ kind: "init", native: true }, term, null);
    s.onMark({ kind: "inputStart" }, term, null);
    expect(s.integrated).toBe(true);
    s.restart();
    expect(s.integrated).toBe(false);
    expect(s.native).toBe(false);
    expect(s.input(term)).toBeNull();
  });
});
