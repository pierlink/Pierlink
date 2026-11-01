import { describe, expect, it } from "vitest";
import { outsideWindow } from "./dnd";

describe("拖出窗口的判断", () => {
  it("窗口里面（含左上角）不算拖出", () => {
    expect(outsideWindow(0, 0, 800, 600)).toBe(false);
    expect(outsideWindow(400, 300, 800, 600)).toBe(false);
    expect(outsideWindow(799.5, 599.5, 800, 600)).toBe(false);
  });

  it("越过任意一边就算拖出", () => {
    expect(outsideWindow(-1, 300, 800, 600)).toBe(true);
    expect(outsideWindow(400, -0.5, 800, 600)).toBe(true);
    expect(outsideWindow(800, 300, 800, 600)).toBe(true);
    expect(outsideWindow(400, 600, 800, 600)).toBe(true);
    expect(outsideWindow(-20, 900, 800, 600)).toBe(true);
  });
});
