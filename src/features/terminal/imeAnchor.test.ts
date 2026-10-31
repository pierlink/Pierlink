import { describe, expect, it } from "vitest";
import { findFakeCursor, type BufferLike } from "./imeAnchor";

/**
 * 用字符串画一屏：每行一个字符串，`#` 是反色格，`W` 是反色的宽字符（占两格，第二格是占位），`w` 是普通宽字符。
 * @param rows 各行
 * @param viewportY 可见区域从第几行开始（前面是回滚区）
 */
function screen(rows: string[], viewportY = 0): BufferLike {
  const lines = rows.map((text) => {
    const cells: { inverse: boolean; width: number }[] = [];
    for (const ch of text) {
      if (ch === "W" || ch === "w") {
        cells.push({ inverse: ch === "W", width: 2 }, { inverse: ch === "W", width: 0 });
      } else cells.push({ inverse: ch === "#", width: 1 });
    }
    return {
      getCell: (x: number) => {
        const c = cells[x];
        return c ? { isInverse: () => (c.inverse ? 1 : 0), getWidth: () => c.width } : undefined;
      },
    };
  });
  return { viewportY, getLine: (y) => lines[y] };
}

describe("找假光标", () => {
  it("屏幕上唯一一个单独的反色格", () => {
    const b = screen(["输出……", "", "╭────────╮", "│ > ab#  │", "╰────────╯", "? for shortcuts"]);
    expect(findFakeCursor(b, 20, 6)).toEqual({ x: 6, y: 3 });
  });

  it("宽字符上的假光标算一格；连成一段的反色不算", () => {
    expect(findFakeCursor(screen(["> 你W好"]), 20, 1)).toEqual({ x: 3, y: 0 });
    expect(findFakeCursor(screen(["#### 选中项", "> abc"]), 20, 2)).toBeNull();
    expect(findFakeCursor(screen(["####", "> ab#"]), 20, 2)).toEqual({ x: 4, y: 1 });
    expect(findFakeCursor(screen(["WW"]), 20, 1)).toBeNull();
  });

  it("没有或不止一个时不挪", () => {
    expect(findFakeCursor(screen(["$ ls", "a b c"]), 20, 2)).toBeNull();
    expect(findFakeCursor(screen(["#  #"]), 20, 1)).toBeNull();
    expect(findFakeCursor(screen(["#", "", "#"]), 20, 3)).toBeNull();
  });

  it("只看可见区域，行号相对可见区域", () => {
    const b = screen(["# 回滚区里的", "line", "> x#"], 1);
    expect(findFakeCursor(b, 20, 2)).toEqual({ x: 3, y: 1 });
  });
});
