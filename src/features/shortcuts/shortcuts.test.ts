import { describe, expect, it } from "vitest";
import zhCN from "../../locales/zh-CN.json";
import en from "../../locales/en.json";
import { DEFAULT_SETTINGS, normalizeSettings } from "../settings/model";
import {
  RESERVED_KEYS, SHORTCUT_ACTIONS, SHORTCUT_GROUPS, checkBinding, normalizeShortcuts, reservedUse, resolveBindings, restoreBlockedBy, withBinding,
  type ShortcutBindings,
} from "./actions";
import { chordFromEvent, comboOf, comboProblem, formatCombo, formatHeld, normalizeCombo, parseCombo, terminalUses, type Chord } from "./keys";
import { setActiveBindings, shortcutOf } from "./runtime";

/** 由规范写法得到按键（测试里少写几个字段） */
const chord = (combo: string): Chord => parseCombo(combo)!;
/** 模拟一次按键事件 */
const keyEvent = (code: string, mods: { ctrl?: boolean; alt?: boolean; shift?: boolean; meta?: boolean; altGr?: boolean } = {}, key = "x") => ({
  code, key, ctrlKey: !!mods.ctrl, altKey: !!mods.alt, shiftKey: !!mods.shift, metaKey: !!mods.meta,
  getModifierState: (k: string) => k === "AltGraph" && !!mods.altGr,
});
const defaults = (): ShortcutBindings => resolveBindings({});

describe("组合键的解析、规范化、显示", () => {
  it("修饰键不分先后和大小写，按键可以写物理位置名或显示的文字", () => {
    expect(normalizeCombo("Ctrl+Shift+KeyO")).toBe("Ctrl+Shift+KeyO");
    expect(normalizeCombo("shift+ctrl+o")).toBe("Ctrl+Shift+KeyO");
    expect(normalizeCombo("Ctrl+=")).toBe("Ctrl+Equal");
    expect(normalizeCombo("ctrl+,")).toBe("Ctrl+Comma");
    expect(normalizeCombo("Ctrl+0")).toBe("Ctrl+Digit0");
    expect(normalizeCombo("Alt+Shift+Ctrl+pgdn")).toBe("Ctrl+Alt+Shift+PageDown");
    expect(normalizeCombo("Win+E")).toBe("Meta+KeyE");
    // “Ctrl++”的“+”在美式键盘上是 Shift+=
    expect(normalizeCombo("Ctrl++")).toBe("Ctrl+Shift+Equal");
  });
  it("不合法的写法得到 null", () => {
    for (const bad of ["", "Ctrl", "Ctrl+Shift", "Ctrl+Ctrl+A", "Ctrl+Foo", "Ctrl++A", "A+Ctrl", "Ctrl+ +A", "x".repeat(100)]) {
      expect(normalizeCombo(bad), bad).toBeNull();
    }
    expect(normalizeCombo(42)).toBeNull();
    expect(normalizeCombo(null)).toBeNull();
    expect(normalizeCombo({ combo: "Ctrl+A" })).toBeNull();
  });
  it("界面上的写法", () => {
    expect(formatCombo("Ctrl+Shift+KeyO")).toBe("Ctrl+Shift+O");
    expect(formatCombo("Ctrl+Equal")).toBe("Ctrl+=");
    expect(formatCombo("Ctrl+Minus")).toBe("Ctrl+-");
    expect(formatCombo("Ctrl+Comma")).toBe("Ctrl+,");
    expect(formatCombo("Ctrl+Shift+Tab")).toBe("Ctrl+Shift+Tab");
    expect(formatCombo("Alt+ArrowLeft")).toBe("Alt+←");
    expect(formatCombo("Ctrl+Meta+KeyK")).toBe("Ctrl+Win+K");
    expect(formatCombo("Ctrl+Shift+KeyT", " ")).toBe("Ctrl Shift T");
    expect(formatCombo(null)).toBe("");
    expect(formatCombo("乱写")).toBe("");
  });
  it("按物理位置识别：输入法开着时 key 是 Process 也认得出", () => {
    const c = chordFromEvent(keyEvent("KeyO", { ctrl: true, shift: true }, "Process"));
    expect(c && comboOf(c)).toBe("Ctrl+Shift+KeyO");
    expect(comboOf(chordFromEvent(keyEvent("Tab", { ctrl: true }, "Tab"))!)).toBe("Ctrl+Tab");
  });
  it("只按修饰键时没有按键；AltGr 打字、读不出物理位置时不算", () => {
    const held = chordFromEvent(keyEvent("ShiftLeft", { ctrl: true, shift: true }, "Shift"))!;
    expect(held.code).toBeNull();
    expect(comboOf(held)).toBeNull();
    expect(formatHeld(held)).toBe("Ctrl+Shift+");
    expect(formatHeld(chordFromEvent(keyEvent("KeyA"))!)).toBe("");
    expect(chordFromEvent(keyEvent("KeyQ", { ctrl: true, alt: true, altGr: true }, "@"))).toBeNull();
    expect(chordFromEvent(keyEvent("", { ctrl: true }, "Unidentified"))).toBeNull();
  });
});

describe("能不能当快捷键", () => {
  it("要带 Ctrl 或 Alt；只加 Shift 是在打字；F 键可以单按；Win 键不行", () => {
    expect(comboProblem({ ctrl: true, alt: false, shift: true, meta: false, code: null })).toBe("modifierOnly");
    expect(comboProblem(chord("KeyA"))).toBe("needModifier");
    expect(comboProblem(chord("Shift+KeyA"))).toBe("needModifier");
    expect(comboProblem(chord("Shift+Tab"))).toBe("needModifier");
    expect(comboProblem(chord("F5"))).toBeNull();
    expect(comboProblem(chord("Ctrl+KeyA"))).toBeNull();
    expect(comboProblem(chord("Alt+KeyA"))).toBeNull();
    expect(comboProblem(chord("Meta+KeyE"))).toBe("winKey");
    expect(comboProblem({ ctrl: true, alt: false, shift: false, meta: false, code: "IntlRo" })).toBe("unsupported");
  });
  it("终端里的程序会用到的键：Ctrl+字母、Alt 开头、F1～F12、Ctrl+_ 等", () => {
    for (const used of ["Ctrl+KeyL", "Ctrl+KeyR", "Ctrl+Space", "Ctrl+BracketLeft", "Alt+KeyB", "Alt+Digit1", "Ctrl+Alt+KeyA", "F2", "Shift+F10", "Ctrl+Shift+Minus", "Ctrl+Shift+Digit2", "Ctrl+ArrowLeft", "Ctrl+Delete", "Alt+Enter"]) {
      expect(terminalUses(chord(used)), used).toBe(true);
    }
    for (const free of ["Ctrl+Shift+KeyL", "Ctrl+Tab", "Ctrl+Shift+Tab", "Ctrl+Equal", "Ctrl+Minus", "Ctrl+Digit0", "Ctrl+Comma", "F13", "Ctrl+Shift+Digit1"]) {
      expect(terminalUses(chord(free)), free).toBe(false);
    }
  });
});

describe("动作表和默认键", () => {
  it("ID 不重复；默认键合法、互不相同、不是保留键、终端里的程序用不到", () => {
    const ids = SHORTCUT_ACTIONS.map((a) => a.id);
    expect(new Set(ids).size).toBe(ids.length);
    const combos: string[] = SHORTCUT_ACTIONS.flatMap((a) => (a.combo === null ? [] : [a.combo]));
    expect(new Set(combos).size).toBe(combos.length);
    for (const c of combos) {
      expect(normalizeCombo(c), c).toBe(c);
      expect(comboProblem(chord(c)), c).toBeNull();
      expect(reservedUse(c), c).toBeNull();
      expect(terminalUses(chord(c)), c).toBe(false);
    }
  });
  it("原来的广播输入快捷键 Ctrl+Shift+B 保留为默认", () => {
    expect(defaults().broadcast).toBe("Ctrl+Shift+KeyB");
  });
  it("默认键不用搜狗、微软拼音的简繁切换 Ctrl+Shift+F（输入法和快捷键会同时动作）", () => {
    expect(Object.values(defaults())).not.toContain("Ctrl+Shift+KeyF");
  });
  it("保留键都是规范写法", () => {
    for (const r of RESERVED_KEYS) expect(normalizeCombo(r.combo), r.combo).toBe(r.combo);
  });
  it("每个动作、分组、保留键用途都有中英文名字", () => {
    for (const lang of [zhCN, en] as Record<string, any>[]) {
      for (const a of SHORTCUT_ACTIONS) expect(lang.shortcuts.action[a.id], a.id).toBeTruthy();
      for (const g of SHORTCUT_GROUPS) expect(lang.shortcuts.group[g], g).toBeTruthy();
      for (const r of RESERVED_KEYS) expect(lang.shortcuts.use[r.use], r.use).toBeTruthy();
      expect(lang.settings.section.shortcuts).toBeTruthy();
    }
  });
});

describe("冲突判断", () => {
  it("和别的动作重复：不能用，说明是哪个动作", () => {
    expect(checkBinding("toggleFiles", chord("Ctrl+Shift+KeyO"), defaults())).toEqual({ ok: false, reason: "taken", combo: "Ctrl+Shift+KeyO", by: "toggleOverview" });
  });
  it("和保留键重复：不能用，说明用途", () => {
    expect(checkBinding("toggleFiles", chord("Ctrl+KeyV"), defaults())).toEqual({ ok: false, reason: "reserved", combo: "Ctrl+KeyV", use: "paste" });
    expect(checkBinding("toggleFiles", chord("Ctrl+Shift+KeyC"), defaults())).toMatchObject({ ok: false, reason: "reserved", use: "copy" });
    // 只加 Shift 的保留键也说明用途（不是笼统的“要带 Ctrl 或 Alt”）
    expect(checkBinding("toggleFiles", chord("Shift+Enter"), defaults())).toMatchObject({ ok: false, reason: "reserved", use: "newline" });
  });
  it("只按修饰键、没带 Ctrl/Alt 的普通字母：不收", () => {
    expect(checkBinding("toggleFiles", { ctrl: true, alt: false, shift: true, meta: false, code: null }, defaults())).toEqual({ ok: false, reason: "modifierOnly" });
    expect(checkBinding("toggleFiles", chord("KeyE"), defaults())).toEqual({ ok: false, reason: "needModifier" });
    expect(checkBinding("toggleFiles", chord("Shift+KeyE"), defaults())).toEqual({ ok: false, reason: "needModifier" });
  });
  it("没人用的键、或者就是自己现在的键：可以", () => {
    expect(checkBinding("toggleFiles", chord("Ctrl+Shift+KeyY"), defaults())).toEqual({ ok: true, combo: "Ctrl+Shift+KeyY" });
    expect(checkBinding("toggleFiles", chord("Ctrl+Shift+KeyE"), defaults())).toEqual({ ok: true, combo: "Ctrl+Shift+KeyE" });
  });
  it("别的动作清空后，它原来的键就能给这个动作用；再恢复那个动作的默认时被挡住", () => {
    const o = withBinding({}, "toggleOverview", null);
    const b = resolveBindings(o);
    expect(b.toggleOverview).toBeNull();
    expect(checkBinding("toggleFiles", chord("Ctrl+Shift+KeyO"), b)).toEqual({ ok: true, combo: "Ctrl+Shift+KeyO" });
    const b2 = resolveBindings(withBinding(o, "toggleFiles", "Ctrl+Shift+KeyO"));
    expect(restoreBlockedBy("toggleOverview", b2)).toBe("toggleFiles");
    expect(restoreBlockedBy("toggleFiles", b2)).toBeNull();
    expect(restoreBlockedBy("goSync", b2)).toBeNull();
  });
});

describe("改动表", () => {
  it("只记和默认不一样的", () => {
    expect(withBinding({}, "toggleFiles", "Ctrl+Shift+KeyY")).toEqual({ toggleFiles: "Ctrl+Shift+KeyY" });
    expect(withBinding({ toggleFiles: "Ctrl+Shift+KeyY" }, "toggleFiles", "Ctrl+Shift+KeyE")).toEqual({});
    expect(withBinding({}, "toggleFiles", null)).toEqual({ toggleFiles: null });
    // 默认没有快捷键的动作：清空等于默认，不用记
    expect(withBinding({ goSync: "Ctrl+Shift+KeyY" }, "goSync", null)).toEqual({});
  });
  it("改过的优先：自己设的键不被别的动作的默认键挤掉", () => {
    const b = resolveBindings({ goSync: "Ctrl+Shift+KeyO" });
    expect(b.goSync).toBe("Ctrl+Shift+KeyO");
    expect(b.toggleOverview).toBeNull();
    expect(b.toggleFiles).toBe("Ctrl+Shift+KeyE");
  });
});

describe("设置的整理", () => {
  it("旧设置没有快捷键这一项：全用默认", () => {
    expect(normalizeSettings({ theme: "light" }).shortcuts).toEqual({});
    expect(DEFAULT_SETTINGS.shortcuts).toEqual({});
    expect(normalizeSettings(undefined)).toEqual(DEFAULT_SETTINGS);
  });
  it("写法整理成规范写法；不认识的动作、不合法的键、保留键、和默认一样的都去掉", () => {
    const s = normalizeSettings({
      shortcuts: {
        toggleFiles: "shift+ctrl+y",
        toggleOverview: null,
        goSync: null,
        newTab: "Ctrl+Shift+KeyT",
        closePane: "Ctrl+KeyV",
        splitRight: "KeyQ",
        splitDown: "Ctrl+Shift",
        snippets: 5,
        bogus: "Ctrl+Shift+KeyZ",
      },
    });
    expect(s.shortcuts).toEqual({ toggleFiles: "Ctrl+Shift+KeyY", toggleOverview: null });
  });
  it("两个动作被手改成同一个键：只留前面那个", () => {
    expect(normalizeShortcuts({ toggleFiles: "Ctrl+Shift+KeyY", goSync: "Ctrl+Shift+KeyY" })).toEqual({ toggleFiles: "Ctrl+Shift+KeyY" });
  });
  it("不是对象时当作没有改动", () => {
    for (const raw of [undefined, null, "Ctrl+A", 3, ["Ctrl+A"]]) expect(normalizeShortcuts(raw)).toEqual({});
  });
});

describe("按键对应的动作", () => {
  it("按现在生效的快捷键查，改了以后旧键不再对应", () => {
    setActiveBindings(defaults());
    const files = keyEvent("KeyE", { ctrl: true, shift: true }, "E") as unknown as KeyboardEvent;
    const y = keyEvent("KeyY", { ctrl: true, shift: true }, "Y") as unknown as KeyboardEvent;
    expect(shortcutOf(files)).toBe("toggleFiles");
    expect(shortcutOf(y)).toBeNull();
    // 终端里的 Ctrl+C、只按 Ctrl 不是快捷键
    expect(shortcutOf(keyEvent("KeyC", { ctrl: true }, "c") as unknown as KeyboardEvent)).toBeNull();
    expect(shortcutOf(keyEvent("ControlLeft", { ctrl: true }, "Control") as unknown as KeyboardEvent)).toBeNull();
    setActiveBindings(resolveBindings({ toggleFiles: "Ctrl+Shift+KeyY" }));
    expect(shortcutOf(files)).toBeNull();
    expect(shortcutOf(y)).toBe("toggleFiles");
    setActiveBindings(defaults());
  });
});
