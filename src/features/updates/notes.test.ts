import { describe, expect, it } from "vitest";
import { pickNotes } from "./notes";

describe("pickNotes", () => {
  const both = "Pierlink 0.6.1\n\n- 中文说明\n\n---\n\nPierlink 0.6.1 (English)\n\n- English notes\n";
  it("中文界面取前一段", () => {
    expect(pickNotes(both, "zh-CN")).toBe("Pierlink 0.6.1\n\n- 中文说明");
  });
  it("英文界面取后一段", () => {
    expect(pickNotes(both, "en")).toBe("Pierlink 0.6.1 (English)\n\n- English notes");
  });
  it("没有分隔线时原样返回", () => {
    expect(pickNotes("- 只有中文\n", "en")).toBe("- 只有中文");
  });
});
