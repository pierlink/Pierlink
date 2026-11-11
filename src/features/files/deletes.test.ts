import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DeleteProgress, FilesApi } from "./api";
import { deleteJobs, dismissDelete, startDelete, stopDelete } from "./deletes";
import { onDirChanged } from "./transfers";

// 假的文件接口（vi.mock 会被提到最前面）：测试里自己发进度、决定结果
const mock = vi.hoisted(() => ({ api: null as unknown }));
vi.mock("./api", () => ({ getFilesApi: async () => mock.api }));

let progress: ((p: DeleteProgress) => void) | null = null;
let settle: { resolve: (n: number) => void; reject: (e: unknown) => void } | null = null;
let calls: { paths: string[]; fast: boolean }[] = [];
let cancelled: string[] = [];

const server = { kind: "server", serverId: "s1" } as const;
/** 等进度合并更新、异步回调跑完 */
const tick = () => new Promise((r) => setTimeout(r, 10));

beforeEach(() => {
  progress = null;
  settle = null;
  calls = [];
  cancelled = [];
  mock.api = {
    remove: (_loc: unknown, paths: string[], fast: boolean, onProgress: (p: DeleteProgress) => void) => {
      calls.push({ paths, fast });
      progress = onProgress;
      return { id: `r${calls.length}`, done: new Promise<number>((resolve, reject) => { settle = { resolve, reject }; }) };
    },
    cancel: async (id: string) => { cancelled.push(id); },
  } as unknown as FilesApi;
  // 测试环境没有 requestAnimationFrame：用定时器代替
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => setTimeout(() => cb(0), 0));
  vi.stubGlobal("cancelAnimationFrame", (id: number) => clearTimeout(id));
});

afterEach(() => {
  for (const j of deleteJobs()) dismissDelete(j.key);
  vi.unstubAllGlobals();
});

describe("删除任务", () => {
  it("逐项删除：浮窗里写已删几项、正在删哪个；删完消失并刷新所在目录", async () => {
    const changed: string[] = [];
    const off = onDirChanged((loc, dir) => changed.push(`${loc} ${dir}`));
    const run = startDelete(server, ["/home/dev/a", "/home/dev/b"], false);
    await tick();
    expect(calls).toEqual([{ paths: ["/home/dev/a", "/home/dev/b"], fast: false }]);
    expect(deleteJobs()).toMatchObject([{ mode: "normal", state: "running", count: 2, deleted: 0, current: null }]);
    progress!({ deleted: 12, current: "/home/dev/a/node_modules/x.js" });
    await tick();
    expect(deleteJobs()[0]).toMatchObject({ deleted: 12, current: "/home/dev/a/node_modules/x.js" });
    settle!.resolve(40);
    await run;
    expect(deleteJobs()).toEqual([]);
    // 两项在同一个目录：只刷新一次
    expect(changed).toEqual(["server:s1 /home/dev"]);
    off();
  });

  it("停下：调用取消，取消后这一项消失；快速删除和本机删除不能停", async () => {
    const run = startDelete(server, ["/home/dev/big"], false);
    await tick();
    const key = deleteJobs()[0].key;
    await stopDelete(key);
    expect(cancelled).toEqual(["r1"]);
    expect(deleteJobs()[0].stopping).toBe(true);
    settle!.reject({ code: "sftp.cancelled", params: {} });
    await run;
    expect(deleteJobs()).toEqual([]);

    const fast = startDelete(server, ["/home/dev/big"], true);
    await tick();
    expect(calls[1]).toEqual({ paths: ["/home/dev/big"], fast: true });
    expect(deleteJobs()[0].mode).toBe("fast");
    await stopDelete(deleteJobs()[0].key);
    expect(cancelled).toEqual(["r1"]);
    settle!.resolve(1);
    await fast;

    const local = startDelete({ kind: "local" }, ["C:\\temp\\a"], false);
    await tick();
    expect(deleteJobs()[0].mode).toBe("local");
    settle!.resolve(1);
    await local;
  });

  it("失败时留着写原因，点 × 关掉", async () => {
    const run = startDelete(server, ["/home/dev/locked"], false);
    await tick();
    settle!.reject({ code: "sftp.permission_denied", params: { path: "/home/dev/locked/x" } });
    await run;
    const [job] = deleteJobs();
    expect(job.state).toBe("failed");
    expect(job.error).toBeTruthy();
    dismissDelete(job.key);
    expect(deleteJobs()).toEqual([]);
  });
});
