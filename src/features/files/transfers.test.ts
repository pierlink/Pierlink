import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { FilesApi } from "./api";
import { cancelTransfer, clearFinishedTransfers, startDragOut, transferJobs } from "./transfers";
import type { DragOutResult, TransferProgressView } from "./types";

// 假的文件接口（vi.mock 会被提到最前面）：测试里自己发拖出的进度、决定结果
const mock = vi.hoisted(() => ({ api: null as unknown }));
vi.mock("./api", () => ({ getFilesApi: async () => mock.api }));

let progress: ((p: TransferProgressView) => void) | null = null;
let settle: { resolve: (r: DragOutResult) => void; reject: (e: unknown) => void } | null = null;
let cancelled: string[] = [];

const zero: TransferProgressView = { totalBytes: 0, doneBytes: 0, current: "", filesDone: 0, filesTotal: 0 };
const from = { kind: "server", serverId: "s1" } as const;
const labels = { title: "Pierlink", line: "正在下载 a.txt", cancelling: "正在取消…" };
/** 等进度合并更新、异步回调跑完 */
const tick = () => new Promise((r) => setTimeout(r, 10));

beforeEach(() => {
  progress = null;
  settle = null;
  cancelled = [];
  mock.api = {
    dragOut: (_from: unknown, _sources: unknown, _labels: unknown, onProgress: (p: TransferProgressView) => void) => {
      progress = onProgress;
      return { id: "d1", done: new Promise<DragOutResult>((resolve, reject) => { settle = { resolve, reject }; }) };
    },
    cancel: async (id: string) => { cancelled.push(id); },
  } as unknown as FilesApi;
  // 测试环境没有 requestAnimationFrame：用定时器代替
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => setTimeout(() => cb(0), 0));
  vi.stubGlobal("cancelAnimationFrame", (id: number) => clearTimeout(id));
});

afterEach(() => {
  clearFinishedTransfers();
  vi.unstubAllGlobals();
});

/** 开始拖出，等它真正调用到接口（包一层返回：async 函数直接返回 Promise 会被展开成等它结束） */
async function start(): Promise<{ run: Promise<DragOutResult> }> {
  const run = startDragOut(from, ["/home/u/a.txt"], "拖出 a.txt", labels);
  run.catch(() => {});
  await tick();
  return { run };
}

describe("拖出到窗口外的传输列表", () => {
  it("没开始下载就结束（没放下、拖回窗口）时列表里不加", async () => {
    const before = transferJobs().length;
    const { run } = await start();
    settle!.resolve({ kind: "cancelled" });
    expect(await run).toEqual({ kind: "cancelled" });
    expect(transferJobs().length).toBe(before);
    const { run: back } = await start();
    settle!.resolve({ kind: "returned", x: 10, y: 20 });
    expect(await back).toEqual({ kind: "returned", x: 10, y: 20 });
    expect(transferJobs().length).toBe(before);
  });

  it("收到第一条进度时加一条，放下后标成完成", async () => {
    const { run } = await start();
    progress!(zero);
    const job = transferJobs().at(-1)!;
    expect(job.state).toBe("running");
    expect(job.progress).toBeNull();
    expect(job.label).toBe("拖出 a.txt");
    expect(job.req.sources).toEqual(["/home/u/a.txt"]);
    progress!({ totalBytes: 100, doneBytes: 40, current: "a.txt", filesDone: 0, filesTotal: 1 });
    await tick();
    expect(transferJobs().at(-1)!.progress?.doneBytes).toBe(40);
    const report = { done: 1, skipped: [], renamed: [], failed: [] };
    settle!.resolve({ kind: "dropped", report });
    await run;
    const done = transferJobs().at(-1)!;
    expect(done.state).toBe("done");
    expect(done.report).toEqual(report);
    expect(done.finishedAt).not.toBeNull();
  });

  it("结果比开始下载的进度先到：照样留一条完成的，迟到的进度不再加", async () => {
    const before = transferJobs().length;
    const { run } = await start();
    const report = { done: 1, skipped: [], renamed: [], failed: [] };
    settle!.resolve({ kind: "dropped", report });
    await run;
    expect(transferJobs().length).toBe(before + 1);
    expect(transferJobs().at(-1)!.state).toBe("done");
    progress!(zero);
    await tick();
    expect(transferJobs().length).toBe(before + 1);
    expect(transferJobs().filter((j) => j.state === "running")).toEqual([]);
  });

  it("下载时取消：标成已取消，不算失败", async () => {
    const { run } = await start();
    progress!(zero);
    settle!.reject({ code: "sftp.cancelled", params: {} });
    await expect(run).rejects.toEqual({ code: "sftp.cancelled", params: {} });
    const job = transferJobs().at(-1)!;
    expect(job.state).toBe("cancelled");
    expect(job.error).toBeNull();
  });

  it("下载出错：标成失败并带原因", async () => {
    const { run } = await start();
    progress!(zero);
    settle!.reject({ code: "files.drag_no_space", params: { need: "2.0 GB", free: "1.0 GB" } });
    await expect(run).rejects.toMatchObject({ code: "files.drag_no_space" });
    const job = transferJobs().at(-1)!;
    expect(job.state).toBe("failed");
    expect(job.error).toBeTruthy();
  });

  it("传输列表里点取消会取消后端的下载", async () => {
    const { run } = await start();
    progress!(zero);
    await cancelTransfer(transferJobs().at(-1)!.key);
    expect(cancelled).toEqual(["d1"]);
    settle!.reject({ code: "sftp.cancelled", params: {} });
    await run.catch(() => {});
  });
});
