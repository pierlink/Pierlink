/**
 * 更新提示的“稍后”：这个版本一天内不再提示；过了一天还没更新再提示一次，出了更新的版本马上提示。
 * 记在本机（每台电脑自己的选择）；存不了时就当没点过“稍后”。
 */
const KEY = "pierlink.updateSnooze";
/** 点“稍后”以后多久再提示同一个版本 */
export const SNOOZE_MS = 24 * 60 * 60 * 1000;

/** 存取“稍后”的地方（测试里换成假的） */
export interface SnoozeStore {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

function local(): SnoozeStore | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage;
  } catch {
    return null;
  }
}

/**
 * 这个版本现在要不要先不提示。
 * @param version 查到的新版本
 * @param now 现在的时间（毫秒）
 * @returns 点过“稍后”、还不到一天时为 true
 */
export function snoozed(version: string, now = Date.now(), store = local()): boolean {
  try {
    const v = JSON.parse(store?.getItem(KEY) ?? "null") as { version?: unknown; until?: unknown } | null;
    // 到期时间比现在晚一天以上：系统时间往回调过，不算（留一小时余量，对时往回调几秒不算）
    return v?.version === version && typeof v.until === "number" && now < v.until && v.until - now <= SNOOZE_MS + 60 * 60 * 1000;
  } catch {
    return false;
  }
}

/**
 * 点了“稍后”：这个版本一天内不再提示。
 * @param version 提示的版本
 * @param now 现在的时间（毫秒）
 */
export function snooze(version: string, now = Date.now(), store = local()): void {
  try {
    store?.setItem(KEY, JSON.stringify({ version, until: now + SNOOZE_MS }));
  } catch { /* 存不了就算了：下次照常提示 */ }
}

/** 用户自己点了“检查更新”：之前的“稍后”不算了 */
export function clearSnooze(store = local()): void {
  try {
    store?.removeItem(KEY);
  } catch { /* 存不了就算了 */ }
}
