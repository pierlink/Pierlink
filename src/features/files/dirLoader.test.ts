import { describe, expect, it } from "vitest";
import { createDirLoader, type DirUpdate } from "./dirLoader";
import type { Listing } from "./types";

/** 一个假的“读目录”：每次调用先挂着，测试里决定哪一次先回来、成功还是失败 */
function setup(cacheMax?: number) {
  const calls: { dir: string; resolve: (l: Listing) => void; reject: (e: unknown) => void }[] = [];
  const updates: DirUpdate[] = [];
  const loader = createDirLoader(
    (dir) => new Promise<Listing>((resolve, reject) => { calls.push({ dir, resolve, reject }); }),
    (u) => updates.push(u),
    cacheMax,
  );
  /** 取走到现在为止的更新 */
  const take = () => updates.splice(0);
  return { loader, calls, take };
}
const listing = (path: string, ...names: string[]): Listing => ({
  path,
  entries: names.map((name) => ({ name, kind: "file", size: 0, mtime: null, hidden: false, targetIsDir: null }) as unknown as Listing["entries"][number]),
  truncated: false,
});
/** 等挂着的回调跑完 */
const tick = () => new Promise((r) => setTimeout(r, 0));

describe("文件面板的目录加载", () => {
  it("没看过的目录：先进去（路径换过去、列表空着），读到了再填上", async () => {
    const { loader, calls, take } = setup();
    const done = loader.load("/home/dev");
    expect(take()).toEqual([
      { kind: "show", path: "/home/dev", listing: null, entered: true, clearSelection: true },
      { kind: "loading", on: true },
    ]);
    const result = listing("/home/dev", "a.txt");
    calls[0].resolve(result);
    await done;
    expect(take()).toEqual([
      { kind: "show", path: "/home/dev", listing: result, entered: false, clearSelection: true },
      { kind: "loading", on: false },
    ]);
  });

  it("看过的目录：先显示记住的列表，后台再读一次；这时选中的项不被清掉", async () => {
    const { loader, calls, take } = setup();
    const first = loader.load("/a");
    const old = listing("/a", "old");
    calls[0].resolve(old);
    await first;
    const away = loader.load("/b");
    calls[1].resolve(listing("/b"));
    await away;
    take();
    const again = loader.load("/a");
    expect(take()).toEqual([
      { kind: "show", path: "/a", listing: old, entered: true, clearSelection: true },
      { kind: "loading", on: true },
    ]);
    const fresh = listing("/a", "old", "new");
    calls[2].resolve(fresh);
    await again;
    expect(take()).toEqual([
      { kind: "show", path: "/a", listing: fresh, entered: false, clearSelection: false },
      { kind: "loading", on: false },
    ]);
  });

  it("连着点进两级：只认最后一次，前面晚到的结果只记下来、不盖掉现在的列表", async () => {
    const { loader, calls, take } = setup();
    const one = loader.load("/a");
    const two = loader.load("/a/b");
    expect(take().filter((u) => u.kind === "show").map((u) => (u.kind === "show" ? u.path : ""))).toEqual(["/a", "/a/b"]);
    // 第一级的结果晚到：界面不动
    const a = listing("/a", "b");
    calls[0].resolve(a);
    await one;
    expect(take()).toEqual([]);
    const b = listing("/a/b", "c.txt");
    calls[1].resolve(b);
    await two;
    expect(take()).toEqual([
      { kind: "show", path: "/a/b", listing: b, entered: false, clearSelection: true },
      { kind: "loading", on: false },
    ]);
    // 回到第一级：刚才晚到的结果已经记下，直接显示
    void loader.load("/a");
    expect(take()[0]).toEqual({ kind: "show", path: "/a", listing: a, entered: true, clearSelection: true });
  });

  it("后点的先回来、先点的后回来：转圈到最后一次读完才停，先点的那次失败也不提示", async () => {
    const { loader, calls, take } = setup();
    const one = loader.load("/slow");
    const two = loader.load("/fast");
    take();
    const fast = listing("/fast", "x");
    calls[1].resolve(fast);
    await two;
    expect(take()).toEqual([
      { kind: "show", path: "/fast", listing: fast, entered: false, clearSelection: true },
      { kind: "loading", on: false },
    ]);
    calls[0].reject({ code: "sftp.permission_denied", params: {} });
    await one;
    expect(take()).toEqual([]);
  });

  it("刷新当前目录：列表不清、选中项留着；同一个目录读了两次，旧的晚到不盖掉新的", async () => {
    const { loader, calls, take } = setup();
    const first = loader.load("/a");
    calls[0].resolve(listing("/a", "1"));
    await first;
    take();
    const r1 = loader.load("/a", true);
    const r2 = loader.load("/a", true);
    expect(take()).toEqual([{ kind: "loading", on: true }, { kind: "loading", on: true }]);
    const newer = listing("/a", "1", "2", "3");
    calls[2].resolve(newer);
    await r2;
    expect(take()).toEqual([
      { kind: "show", path: "/a", listing: newer, entered: false, clearSelection: false },
      { kind: "loading", on: false },
    ]);
    calls[1].resolve(listing("/a", "1", "2"));
    await r1;
    expect(take()).toEqual([]);
    // 记住的是新的那份
    void loader.load("/b");
    take();
    void loader.load("/a");
    expect(take()[0]).toMatchObject({ kind: "show", path: "/a", listing: newer });
  });

  it("读不出来：没看过的目录显示原因（人已经在那个目录里）；看过的留着旧列表、另外提示", async () => {
    const { loader, calls, take } = setup();
    const denied = { code: "sftp.permission_denied", params: {} };
    const bad = loader.load("/root");
    calls[0].reject(denied);
    await bad;
    expect(take()).toEqual([
      { kind: "show", path: "/root", listing: null, entered: true, clearSelection: true },
      { kind: "loading", on: true },
      { kind: "error", path: "/root", error: denied },
      { kind: "loading", on: false },
    ]);
    const ok = loader.load("/a");
    const a = listing("/a", "f");
    calls[1].resolve(a);
    await ok;
    const away = loader.load("/b");
    calls[2].resolve(listing("/b"));
    await away;
    take();
    const again = loader.load("/a");
    calls[3].reject(denied);
    await again;
    expect(take()).toEqual([
      { kind: "show", path: "/a", listing: a, entered: true, clearSelection: true },
      { kind: "loading", on: true },
      { kind: "refreshFailed", error: denied },
      { kind: "loading", on: false },
    ]);
  });

  it("服务器把路径解析成别的写法：读到后换成实际的路径；两种写法都记住", async () => {
    const { loader, calls, take } = setup();
    const done = loader.load("/srv/link");
    const real = listing("/data/real", "f");
    calls[0].resolve(real);
    await done;
    expect(take().filter((u) => u.kind === "show")).toEqual([
      { kind: "show", path: "/srv/link", listing: null, entered: true, clearSelection: true },
      { kind: "show", path: "/data/real", listing: real, entered: true, clearSelection: true },
    ]);
    void loader.load("/elsewhere");
    take();
    void loader.load("/srv/link");
    expect(take()[0]).toMatchObject({ kind: "show", path: "/data/real", listing: real });
    await tick();
  });

  it("只记住最近的若干个目录", async () => {
    const { loader, calls, take } = setup(2);
    for (const [i, dir] of ["/1", "/2", "/3"].entries()) {
      const done = loader.load(dir);
      calls[i].resolve(listing(dir));
      await done;
    }
    take();
    void loader.load("/1");
    // 最早的那个已经不记得了：先进去、列表空着
    expect(take()[0]).toEqual({ kind: "show", path: "/1", listing: null, entered: true, clearSelection: true });
    void loader.load("/3");
    expect(take()[0]).toMatchObject({ kind: "show", path: "/3", entered: true });
    expect(take()).toEqual([]);
    await tick();
  });
});
