import type { Listing } from "./types";

/** 记住多少个看过的目录 */
const CACHE_MAX = 100;

/** 读目录过程中要界面做的事 */
export type DirUpdate =
  /**
   * 显示这个目录。listing 为 null：刚点进一个没看过的目录，路径先换过去、列表先空着（显示“正在读取”），读到了再来一次。
   * entered：换了目录（不是同一个目录的刷新）；clearSelection：选中项要清掉。
   */
  | { kind: "show"; path: string; listing: Listing | null; entered: boolean; clearSelection: boolean }
  | { kind: "loading"; on: boolean }
  /** 这个目录读不出来，也没有记住的列表可显示 */
  | { kind: "error"; path: string; error: unknown }
  /** 后台重新读失败：记住的列表留着，另外提示一下 */
  | { kind: "refreshFailed"; error: unknown };

export interface DirLoader {
  /**
   * 读目录。
   * 进出目录（force 为 false）：看过的目录先显示记住的列表，后台再读一次；没看过的先进去（列表空着），读到了再填上。
   * 刷新、改动之后（force 为 true）：列表不清，只显示读到的结果。
   * 连着点了几个目录时只认最后一次：前面晚到的结果只记下来（下次进去直接有），不会盖掉现在的列表。
   * @param dir 要读的目录
   * @param keepSelection 保留选中项（刷新时）
   * @param force 不用记住的列表
   */
  load(dir: string, keepSelection?: boolean, force?: boolean): Promise<void>;
}

/**
 * 文件面板的目录加载：谁先谁后、记住看过的目录、先进去再读。和界面无关，界面按 apply 收到的更新照做。
 * 一台机器用一个（记住的列表不能串到别的机器上）；换了机器另建一个，旧的那个晚到的更新由界面丢掉。
 * @param list 读一个目录
 * @param apply 界面要做的更新
 * @param cacheMax 记住多少个目录
 */
export function createDirLoader(list: (dir: string) => Promise<Listing>, apply: (update: DirUpdate) => void, cacheMax = CACHE_MAX): DirLoader {
  /** 记住的列表，和它是第几次 load 读到的（同一个目录先后读了两次、先读的反而晚到时，不让旧的盖掉新的） */
  const cache = new Map<string, { listing: Listing; at: number }>();
  /** 每次 load 加一：只有最新的这次能改界面 */
  let seq = 0;
  /** 界面上现在是哪个目录 */
  let shown: string | null = null;
  const remember = (dir: string, result: Listing, at: number) => {
    for (const key of new Set([dir, result.path])) {
      if ((cache.get(key)?.at ?? 0) > at) continue;
      cache.delete(key);
      cache.set(key, { listing: result, at });
    }
    while (cache.size > cacheMax) cache.delete(cache.keys().next().value as string);
  };
  return {
    async load(dir, keepSelection = false, force = keepSelection) {
      const my = ++seq;
      const cached = force ? undefined : cache.get(dir)?.listing;
      if (cached) {
        const entered = cached.path !== shown;
        shown = cached.path;
        apply({ kind: "show", path: cached.path, listing: cached, entered, clearSelection: !keepSelection });
      } else if (!force && dir !== shown) {
        // 先进去：路径马上换成要进的目录，列表先空着
        shown = dir;
        apply({ kind: "show", path: dir, listing: null, entered: true, clearSelection: true });
      }
      apply({ kind: "loading", on: true });
      try {
        const result = await list(dir);
        // 晚到的结果也记下：下次进这个目录直接有
        remember(dir, result, my);
        if (my !== seq) return;
        const entered = result.path !== shown;
        shown = result.path;
        apply({ kind: "show", path: result.path, listing: result, entered, clearSelection: !keepSelection && !cached });
      } catch (error) {
        if (my !== seq) return;
        if (cached) { apply({ kind: "refreshFailed", error }); return; }
        shown = dir;
        apply({ kind: "error", path: dir, error });
      } finally {
        if (my === seq) apply({ kind: "loading", on: false });
      }
    },
  };
}
