import { afterEach, describe, expect, it } from "vitest";
import { clearHistory, historyOf, remember, setHistoryApi, type HistoryApi } from "./history";
import type { HistoryEntry } from "./predict";

const entry = (command: string, at: number): HistoryEntry => ({ command, cwd: null, exit: 0, at });
const flush = () => new Promise((r) => setTimeout(r, 0));

function fakeApi(stored: HistoryEntry[], fail = false): HistoryApi & { calls: number; added: HistoryEntry[] } {
  const api = {
    calls: 0,
    added: [] as HistoryEntry[],
    async list() { api.calls++; if (fail) throw new Error("x"); return stored; },
    async add(_scope: string, e: HistoryEntry) { api.added.push(e); },
    async clear() { stored = []; },
  };
  return api;
}

afterEach(() => setHistoryApi(null));

describe("命令历史缓存", () => {
  it("先记下一条再预测：本机存的历史照样读进来，放在新记录前面", async () => {
    const api = fakeApi([entry("git status", 1)]);
    setHistoryApi(api);
    remember("server:a", entry("ls -la", 2));
    await flush();
    expect(historyOf("server:a").map((e) => e.command)).toEqual(["git status", "ls -la"]);
    expect(api.added.map((e) => e.command)).toEqual(["ls -la"]);
    expect(api.calls).toBe(1);
  });

  it("读失败不每次重试", async () => {
    const api = fakeApi([], true);
    setHistoryApi(api);
    historyOf("server:a");
    await flush();
    historyOf("server:a");
    historyOf("server:a");
    expect(api.calls).toBe(1);
  });

  it("像是带口令的命令不记；清除后重新从后端读", async () => {
    const api = fakeApi([entry("git status", 1)]);
    setHistoryApi(api);
    historyOf("server:a");
    await flush();
    remember("server:a", entry("mysql --password=x", 2));
    expect(api.added).toEqual([]);
    expect(historyOf("server:a").map((e) => e.command)).toEqual(["git status"]);
    await clearHistory();
    expect(historyOf("server:a")).toEqual([]);
    await flush();
    expect(historyOf("server:a")).toEqual([]);
    expect(api.calls).toBe(2);
  });

  it("清除时还在读的旧记录不回到缓存；清除期间不读，清完再读", async () => {
    let release: (v: HistoryEntry[]) => void = () => undefined;
    let calls = 0;
    let cleared: () => void = () => undefined;
    setHistoryApi({
      list: () => { calls++; return calls === 1 ? new Promise((r) => { release = r; }) : Promise.resolve([]); },
      add: async () => undefined,
      clear: () => new Promise((r) => { cleared = r; }),
    });
    historyOf("server:a");
    const done = clearHistory();
    release([entry("old", 1)]);
    await flush();
    expect(historyOf("server:a")).toEqual([]);
    expect(calls).toBe(1);
    cleared();
    await done;
    historyOf("server:a");
    await flush();
    expect(calls).toBe(2);
    expect(historyOf("server:a")).toEqual([]);
  });

  it("刚记下的一条先存进后端又被读到时只留一份", async () => {
    const stored: HistoryEntry[] = [entry("git status", 1)];
    let release: () => void = () => undefined;
    setHistoryApi({
      list: () => new Promise((r) => { release = () => r([...stored]); }),
      add: async (_s, e) => { stored.push(e); },
      clear: async () => undefined,
    });
    remember("server:a", entry("ls", 2));
    await flush();
    release();
    await flush();
    expect(historyOf("server:a").map((e) => e.command)).toEqual(["git status", "ls"]);
  });

  it("没有后端时只存在内存里", () => {
    setHistoryApi(null);
    remember("local:bash", entry("make", 1));
    expect(historyOf("local:bash").map((e) => e.command)).toEqual(["make"]);
  });
});
