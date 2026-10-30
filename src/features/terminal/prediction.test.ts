import { describe, expect, it } from "vitest";
import { fit } from "./prediction";

describe("预测文字按格截断", () => {
  it("放得下时原样显示", () => {
    expect(fit("abc", 3)).toEqual({ text: "abc", width: 3 });
  });
  it("放不下时最后一格换成省略号（按 → 会填入整条，先让人知道后面还有）", () => {
    expect(fit("abcdef", 4)).toEqual({ text: "abc…", width: 4 });
    expect(fit("abc", 1)).toEqual({ text: "…", width: 1 });
  });
  it("宽字符占两格", () => {
    expect(fit("中文命令", 5)).toEqual({ text: "中文…", width: 5 });
    expect(fit("中文", 4)).toEqual({ text: "中文", width: 4 });
  });
  it("一格都没有时不显示", () => {
    expect(fit("abc", 0)).toEqual({ text: "", width: 0 });
  });
});
