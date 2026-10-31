import { isSensitive, type HistoryEntry } from "./predict";
import { inTauri } from "../shell/TitleBar";

/**
 * 命令历史的内存缓存：每个范围（服务器、快速连接、本机 Git Bash）第一次用到时从后端整段读进来，
 * 同一台服务器的几个窗格共用一份；新记录先进缓存，再异步存到后端。
 * 保留策略在后端（每个范围最新 5000 条、180 天）；这里只把缓存限制在同样的条数。
 */

/** 后端的命令历史接口（浏览器预览时没有，只存在内存里） */
export interface HistoryApi {
  list(scope: string): Promise<HistoryEntry[]>;
  add(scope: string, entry: HistoryEntry): Promise<void>;
  clear(): Promise<void>;
}

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 运行在 Tauri 窗口里时存到后端（应用数据库）；用到时才加载后端命令 */
function backendApi(): HistoryApi {
  const commands = async () => (await import("../../bindings/commands")).commands;
  return {
    // 时间在后端是整数毫秒，不会是空；类型上可能为空，按 0 处理
    async list(scope) { return unwrap(await (await commands()).historyList(scope)).map((e) => ({ ...e, at: e.at ?? 0 })); },
    async add(scope, entry) { unwrap(await (await commands()).historyAdd(scope, entry)); },
    async clear() { unwrap(await (await commands()).historyClear()); },
  };
}

const MAX = 5000;
const cache = new Map<string, HistoryEntry[]>();
const loading = new Set<string>();
/** 已经从后端读过的范围（读失败也算，免得每一帧都重试） */
const loaded = new Set<string>();
const listeners = new Set<() => void>();
// 浏览器预览没有后端：只存在内存里，刷新页面就没了
let api: HistoryApi | null = inTauri ? backendApi() : null;
// 换接口或清除记录时加一：之前发出、还没回来的读取作废（否则清掉的记录会回到缓存）
let generation = 0;
// 正在清除后端的记录：这期间不读，清完再读
let clearing: Promise<void> | null = null;

/** 换掉后端接口（测试里换成假的） */
export function setHistoryApi(next: HistoryApi | null): void {
  api = next;
  generation++;
  clearing = null;
  cache.clear();
  loading.clear();
  loaded.clear();
}

/** 同一条记录（同一毫秒记下的同一条命令）只留一份 */
const key = (e: HistoryEntry) => `${e.at}\u0000${e.command}`;

/** 从后端读到的记录放前面，读之前、读的过程中新记下的放后面；新记下的已经存进后端、读到了的不重复放 */
function merge(stored: HistoryEntry[], pending: HistoryEntry[]): HistoryEntry[] {
  const seen = new Set(stored.map(key));
  return [...stored, ...pending.filter((e) => !seen.has(key(e)))].slice(-MAX);
}

/** 历史读进来后通知（让正在输入的终端重新算预测） */
export function onHistoryLoaded(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/**
 * 某个范围的历史（按时间先后）。还没读进来时先返回空，读完后通知。
 * @param scope 范围键
 */
export function historyOf(scope: string): readonly HistoryEntry[] {
  if (api && !clearing && !loaded.has(scope) && !loading.has(scope)) {
    loading.add(scope);
    const target = api;
    const gen = generation;
    target.list(scope).then((entries) => {
      if (gen !== generation) return;
      cache.set(scope, merge(entries, cache.get(scope) ?? []));
      listeners.forEach((l) => l());
    }, () => undefined).finally(() => {
      if (gen !== generation) return;
      loading.delete(scope);
      loaded.add(scope);
    });
  }
  return cache.get(scope) ?? [];
}

/**
 * 记下一条执行过的命令。可能带口令、令牌的不记。
 * @param scope 范围键
 * @param entry 命令记录
 */
export function remember(scope: string, entry: HistoryEntry): void {
  if (isSensitive(entry.command)) return;
  // 先触发读取：这个范围的第一条记录比预测先到时（例如交给 AI 期间），也要把本机存的历史读进来
  historyOf(scope);
  const list = cache.get(scope) ?? [];
  list.push(entry);
  if (list.length > MAX + 200) list.splice(0, list.length - MAX);
  cache.set(scope, list);
  void api?.add(scope, entry).catch(() => undefined);
}

/** 清除全部命令记录（设置里的“清除命令记录”） */
export async function clearHistory(): Promise<void> {
  generation++;
  cache.clear();
  loading.clear();
  loaded.clear();
  const job = (async () => { await api?.clear(); })();
  clearing = job;
  try {
    await job;
  } finally {
    if (clearing === job) {
      clearing = null;
      // 清除期间没读的范围现在可以读了：让正在输入的终端重新算
      listeners.forEach((l) => l());
    }
  }
}
