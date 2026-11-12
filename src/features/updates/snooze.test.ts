import { describe, expect, it } from "vitest";
import { SNOOZE_MS, clearSnooze, snooze, snoozed, type SnoozeStore } from "./snooze";

function fakeStore(): SnoozeStore & { data: Map<string, string> } {
  const data = new Map<string, string>();
  return {
    data,
    getItem: (k) => data.get(k) ?? null,
    setItem: (k, v) => { data.set(k, v); },
    removeItem: (k) => { data.delete(k); },
  };
}

describe("更新提示的“稍后”", () => {
  it("同一个版本一天内不再提示，过了一天、换了版本照常提示", () => {
    const store = fakeStore();
    const now = 1_000_000;
    expect(snoozed("1.2.11", now, store)).toBe(false);
    snooze("1.2.11", now, store);
    expect(snoozed("1.2.11", now + 1, store)).toBe(true);
    expect(snoozed("1.2.11", now + SNOOZE_MS - 1, store)).toBe(true);
    expect(snoozed("1.2.11", now + SNOOZE_MS, store)).toBe(false);
    // 出了更新的版本：马上提示
    expect(snoozed("1.2.12", now + 1, store)).toBe(false);
  });

  it("系统时间往回调过（到期比现在晚一天以上）时照常提示", () => {
    const store = fakeStore();
    const now = 10 * SNOOZE_MS;
    snooze("1.2.11", now, store);
    expect(snoozed("1.2.11", now + 1, store)).toBe(true);
    // 对时往回调了几秒：还算
    expect(snoozed("1.2.11", now - 1000, store)).toBe(true);
    // 点了“稍后”以后系统时间往回调了两小时：不算
    expect(snoozed("1.2.11", now - 2 * 60 * 60 * 1000, store)).toBe(false);
  });

  it("自己点了检查更新就不算了；存的东西坏了或没地方存时照常提示", () => {
    const store = fakeStore();
    snooze("1.2.11", 0, store);
    clearSnooze(store);
    expect(snoozed("1.2.11", 1, store)).toBe(false);
    store.data.set("pierlink.updateSnooze", "不是 JSON");
    expect(snoozed("1.2.11", 1, store)).toBe(false);
    expect(snoozed("1.2.11", 1, null)).toBe(false);
    expect(() => snooze("1.2.11", 1, null)).not.toThrow();
  });
});
