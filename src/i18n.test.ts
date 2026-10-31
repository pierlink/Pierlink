import { describe, expect, it } from "vitest";
import zhCN from "./locales/zh-CN.json";
import en from "./locales/en.json";
import { pickLanguage } from "./i18n";

/** 把嵌套的翻译对象展开成 “a.b.c” 形式的键列表 */
function keys(obj: Record<string, unknown>, prefix = ""): string[] {
  return Object.entries(obj).flatMap(([k, v]) =>
    v && typeof v === "object" ? keys(v as Record<string, unknown>, `${prefix}${k}.`) : [`${prefix}${k}`],
  );
}

/** 取出嵌套对象里某个键的值 */
function valueAt(obj: Record<string, unknown>, key: string): unknown {
  return key.split(".").reduce<unknown>((x, p) => (x as Record<string, unknown>)[p], obj);
}

/** 复数形式（如英文的 x_one / x_other）按同一个键比较；中文没有复数变化 */
function baseKeys(obj: Record<string, unknown>): string[] {
  return [...new Set(keys(obj).map((k) => k.replace(/_(zero|one|two|few|many|other)$/, "")))].sort();
}

describe("多语言资源", () => {
  it("中英文的键完全一致", () => {
    const a = baseKeys(zhCN);
    const b = baseKeys(en);
    expect(a.filter((k) => !b.includes(k))).toEqual([]);
    expect(b.filter((k) => !a.includes(k))).toEqual([]);
  });

  it("英文的复数形式成对出现", () => {
    const plural = keys(en).filter((k) => /_(one|other)$/.test(k));
    for (const k of plural) {
      const base = k.replace(/_(one|other)$/, "");
      expect(keys(en)).toContain(`${base}_one`);
      expect(keys(en)).toContain(`${base}_other`);
    }
  });

  it("没有空字符串", () => {
    expect(keys(zhCN).filter((k) => valueAt(zhCN, k) === "")).toEqual([]);
    expect(keys(en).filter((k) => valueAt(en, k) === "")).toEqual([]);
  });

  it("按系统语言选择界面语言", () => {
    expect(pickLanguage("zh-CN")).toBe("zh-CN");
    expect(pickLanguage("zh-TW")).toBe("zh-CN");
    expect(pickLanguage("en-US")).toBe("en");
    expect(pickLanguage(undefined)).toBe("en");
  });
});
