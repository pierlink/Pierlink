import { Channel } from "@tauri-apps/api/core";
import { commands, type LocDto, type TransferProgressDto } from "../../bindings/commands";
import { formatError } from "../../errors";
import type { FilesApi } from "./api";
import type { EntryKind, FileVersion, Loc, TransferRequest } from "./types";

/** tauri-specta 对可能失败的命令返回 {status, data|error}；这里统一转成“成功返回数据、失败抛出错误” */
function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

function locDto(loc: Loc): LocDto {
  if (loc.kind === "local") return { kind: "local" };
  if (loc.kind === "server") return { kind: "server", serverId: loc.serverId };
  return { kind: "quick", host: loc.host, port: loc.port, user: loc.user };
}

function reqDto(req: TransferRequest) {
  return { from: locDto(req.from), sources: req.sources, to: locDto(req.to), destDir: req.destDir };
}

/** 后端的文件版本（数字字段生成的类型可能为 null）→ 界面用的版本 */
function version(v: { size: number | null; mtime: number | null; hash: string | null }): FileVersion {
  return { size: v.size ?? 0, mtime: v.mtime ?? 0, hash: v.hash };
}

/** Tauri 后端的文件接口 */
export function createTauriFilesApi(): FilesApi {
  let seq = 0;
  return {
    preview: false,
    async home(loc) { return unwrap(await commands.filesHome(locDto(loc))); },
    async list(loc, path) {
      const r = unwrap(await commands.filesList(locDto(loc), path));
      return { path: r.path, truncated: r.truncated, entries: r.entries.map((e) => ({ ...e, size: e.size ?? 0, mtime: e.mtime ?? 0, kind: e.kind as EntryKind })) };
    },
    async terminalCwd(sessionId) { return unwrap(await commands.filesTerminalCwd(sessionId)); },
    async plan(req) {
      const r = unwrap(await commands.filesPlan(reqDto(req)));
      return { files: r.files ?? 0, bytes: r.bytes ?? 0, conflicts: r.conflicts };
    },
    transfer(req, conflict, onProgress) {
      // 传输 ID 由前端生成：命令要等传输结束才返回，取消需要提前知道 ID
      const id = `x${Date.now().toString(36)}-${++seq}`;
      const channel = new Channel<TransferProgressDto>();
      channel.onmessage = (p) => onProgress({ ...p, totalBytes: p.totalBytes ?? 0, doneBytes: p.doneBytes ?? 0 });
      const done = commands.filesTransfer(id, reqDto(req), conflict, channel).then(unwrap).then((r) => ({
        done: r.done,
        skipped: r.skipped,
        renamed: r.renamed,
        failed: r.failed.map((f) => ({ path: f.path, reason: formatError(f.reason) })),
      }));
      return { id: Promise.resolve(id), done };
    },
    async cancel(id) { await commands.filesCancel(id); },
    async rename(loc, from, to) { unwrap(await commands.filesRename(locDto(loc), from, to)); },
    async createDir(loc, path) { unwrap(await commands.filesCreateDir(locDto(loc), path)); },
    async createFile(loc, path) { unwrap(await commands.filesCreateFile(locDto(loc), path)); },
    async remove(loc, paths) { return unwrap(await commands.filesDelete(locDto(loc), paths)) ?? 0; },
    async readText(loc, path) {
      const r = unwrap(await commands.filesReadText(locDto(loc), path));
      return { text: r.text, bom: r.bom, version: version(r.version) };
    },
    async writeText(loc, path, text, bom, expected) { return version(unwrap(await commands.filesWriteText(locDto(loc), path, text, bom, expected))); },
    async mode(loc, path) { return unwrap(await commands.filesMode(locDto(loc), path)); },
    async chmod(loc, path, mode) { unwrap(await commands.filesChmod(locDto(loc), path, mode)); },
    async openDefault(path) { unwrap(await commands.filesOpenDefault(path)); },
    async reveal(path) { unwrap(await commands.filesReveal(path)); },
  };
}
