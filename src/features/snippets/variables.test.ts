import { describe, expect, it } from "vitest";
import { normalizeNewlines, parseVariables, prepareForSend, render } from "./variables";

describe("片段变量", () => {
  it("按出现顺序去重，支持中文和 - _，容许花括号里的空格", () => {
    expect(parseVariables("git switch {{branch}} && echo {{ branch }} {{环境}} {{a-b_c}}")).toEqual(["branch", "环境", "a-b_c"]);
  });
  it("不是变量的花括号不算", () => {
    expect(parseVariables("echo {a} {{}} {{有 空格}} ${HOME}")).toEqual([]);
  });
  it("替换变量，没填的保留原样，值里的换行统一", () => {
    expect(render("cd {{dir}} && ls {{x}}", { dir: "/tmp/a b" })).toBe("cd /tmp/a b && ls {{x}}");
    expect(render("echo {{v}}", { v: "1\r\n2" })).toBe("echo 1\n2");
  });
  it("不做 shell 转义", () => {
    expect(render("rm {{f}}", { f: "*.log; echo $HOME" })).toBe("rm *.log; echo $HOME");
  });
});

describe("换行", () => {
  it("CRLF、CR 都变成 LF", () => {
    expect(normalizeNewlines("a\r\nb\rc\nd")).toBe("a\nb\nc\nd");
  });
  it("发送前去掉末尾空行", () => {
    expect(prepareForSend("ls\r\n\r\n")).toBe("ls");
    expect(prepareForSend("a\r\nb")).toBe("a\nb");
  });
});
