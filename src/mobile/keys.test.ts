import { describe, expect, it } from "vitest";
import { DEFAULT_PRESETS, KEY_ROWS, MODS, parsePresets, presetLabel, startsWithSubmit, stepFor, toRequests, type KeyStep } from "./keys";

describe("手机上的组合键", () => {
  it("不带修饰键：特殊键是按键，字母数字空格是直接打的字", () => {
    expect(stepFor([], "Escape")).toEqual({ key: "Escape" });
    expect(stepFor([], "F5")).toEqual({ key: "F5" });
    expect(stepFor([], "d")).toEqual({ text: "d" });
    expect(stepFor([], "Space")).toEqual({ text: " " });
    expect(stepFor([], "]")).toEqual({ text: "]" });
  });

  it("带修饰键：只给电脑端认得的组合，Shift+字母是大写字母", () => {
    expect(stepFor(["Ctrl"], "a")).toEqual({ key: "Ctrl+A" });
    expect(stepFor(["Alt"], "b")).toEqual({ key: "Alt+B" });
    expect(stepFor(["Shift"], "g")).toEqual({ text: "G" });
    expect(stepFor(["Ctrl"], "ArrowLeft")).toEqual({ key: "Ctrl+ArrowLeft" });
    expect(stepFor(["Shift"], "Tab")).toEqual({ key: "Shift+Tab" });
    expect(stepFor(["Ctrl"], "\\")).toEqual({ key: "Ctrl+\\" });
    expect(stepFor(["Ctrl"], "F5")).toBeNull();
    expect(stepFor(["Alt"], "1")).toBeNull();
    expect(stepFor(["Ctrl", "Shift"], "a")).toBeNull();
  });

  it("面板上每个键在每个修饰键下要么能发、要么置灰，名字都能存下来再读出", () => {
    const steps: KeyStep[] = [];
    for (const mods of [[], ...MODS.map((m) => [m])]) {
      for (const base of KEY_ROWS.flat()) {
        const step = stepFor(mods, base);
        if (step) steps.push(step);
      }
    }
    expect(steps.length).toBeGreaterThan(100);
    for (const step of steps) expect(parsePresets([{ steps: [step] }])).toEqual([{ steps: [step] }]);
  });

  it("显示的字：Ctrl 写成 ^，连着几步用空格隔开", () => {
    expect(presetLabel([{ key: "Ctrl+A" }, { text: "d" }])).toBe("^A d");
    expect(presetLabel([{ key: "Shift+Tab" }])).toBe("⇧Tab");
    expect(presetLabel([{ key: "Alt+ArrowLeft" }])).toBe("Alt+←");
    expect(presetLabel([{ key: "Escape" }, { text: " " }])).toBe("Esc ␣");
  });

  it("发出去：连着的按键合成一次，字单独一次", () => {
    expect(toRequests([{ key: "Ctrl+A" }, { text: "d" }])).toEqual([{ keys: ["Ctrl+A"] }, { text: "d" }]);
    expect(toRequests([{ key: "Escape" }, { key: "Escape" }, { text: ":" }, { text: "q" }, { key: "Enter" }]))
      .toEqual([{ keys: ["Escape", "Escape"] }, { text: ":q" }, { keys: ["Enter"] }]);
    // 回车不和前面的键合成一次：电脑要先看到 ↑ 调出来的命令才能核对
    expect(toRequests([{ key: "ArrowUp" }, { key: "Enter" }, { key: "ArrowUp" }, { key: "ArrowUp" }, { key: "Ctrl+M" }]))
      .toEqual([{ keys: ["ArrowUp"] }, { keys: ["Enter", "ArrowUp", "ArrowUp"] }, { keys: ["Ctrl+M"] }]);
    expect(toRequests([{ key: "Enter" }, { key: "Enter" }]).map(startsWithSubmit)).toEqual([true, true]);
    expect(startsWithSubmit({ keys: ["ArrowUp"] })).toBe(false);
    expect(startsWithSubmit({ text: "x" })).toBe(false);
    // 合并时不改存着的那一组
    const preset = DEFAULT_PRESETS[2].steps;
    toRequests(preset);
    expect(preset).toEqual([{ key: "Ctrl+A" }, { text: "d" }]);
  });

  it("读存下来的：格式不对的丢掉，不是数组当成没存过", () => {
    expect(parsePresets(undefined)).toBeNull();
    expect(parsePresets([])).toEqual([]);
    expect(parsePresets([{ steps: [] }, { steps: [{ key: "Ctrl+A" }, { text: "d" }] }, { steps: [{ text: "\x1b[A" }] }, { steps: [{ key: "<script>" }] }, null]))
      .toEqual([{ steps: [{ key: "Ctrl+A" }, { text: "d" }] }]);
  });
});
