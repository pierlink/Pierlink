import { Channel } from "@tauri-apps/api/core";
import { commands, type TerminalEventDto_Deserialize } from "../../bindings/commands";
import { base64ToBytes } from "./util";
import type { TerminalBackend } from "./types";
import type { LocalShellView } from "./backend";

/** tauri-specta 对可能失败的命令返回 {status, data|error}；这里统一转成“成功返回数据、失败抛出错误” */
function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 运行在 Tauri 窗口里时使用的真实终端后端 */
export function createTauriBackend(): TerminalBackend {
  // 每个会话的写入排成一队：Tauri 的异步命令会并发执行，连续两次写入（例如粘贴后紧跟一个回车）
  // 可能后发先到，回车先到就会执行半截命令。上一次写完（成功或失败）才发下一次
  const queues = new Map<string, Promise<unknown>>();
  return {
    preview: false,
    async open(spec, cols, rows, onEvent, prediction = false) {
      // 先建立接收通道再发起打开：后端可能在命令返回之前就开始推送输出
      const channel = new Channel<TerminalEventDto_Deserialize>();
      channel.onmessage = (e) => {
        if (e.kind === "output") onEvent({ kind: "output", data: base64ToBytes(e.data) });
        else onEvent({ kind: "exited", code: e.code ?? null, closedByUser: e.closedByUser, auth: e.auth ?? null });
      };
      if (spec.kind === "local") return unwrap(await commands.terminalOpenLocal(spec.shellId, cols, rows, prediction, channel));
      if (spec.kind === "server") return unwrap(await commands.terminalOpenServer(spec.serverId, cols, rows, prediction, channel));
      return unwrap(await commands.terminalOpenQuick(spec.host, spec.port, spec.user, cols, rows, prediction, channel));
    },
    write(id, data) {
      const next = (queues.get(id) ?? Promise.resolve())
        .catch(() => {})
        .then(async () => { unwrap(await commands.terminalWrite(id, data)); });
      queues.set(id, next);
      // 队列尾部完成后清掉，避免会话关闭后一直占着
      void next.finally(() => { if (queues.get(id) === next) queues.delete(id); }).catch(() => {});
      return next;
    },
    async resize(id, cols, rows) { unwrap(await commands.terminalResize(id, cols, rows)); },
    async ack(id, bytes) { unwrap(await commands.terminalAck(id, bytes)); },
    async close(id) { unwrap(await commands.terminalClose(id)); },
    async busy(id) { return unwrap(await commands.terminalBusy(id)); },
    async checkHostKey(target) {
      if (target.kind === "server") return unwrap(await commands.hostKeyCheckServer(target.serverId));
      return unwrap(await commands.hostKeyCheck(target.host, target.port, target.user));
    },
    async answerHostKey(probeId, accept) { return unwrap(await commands.hostKeyAnswer(probeId, accept)); },
    async cancelHostKey(probeId) { await commands.hostKeyCancel(probeId); },
  };
}

/** 后端检测到的本机终端 */
export async function listLocalShellsFromBackend(): Promise<LocalShellView[]> {
  return unwrap(await commands.listLocalShells());
}
