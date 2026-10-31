import type { AppError } from "../../bindings/commands";
import { inTauri } from "../shell/TitleBar";

/** 后端推来的内置 SSH 连接状态（事件 pierlink://connection-state，每台服务器的最新状态） */
export interface ConnectionStateEvent {
  server_id: string;
  state: "connecting" | "connected" | "reconnecting" | "waiting_password" | "waiting_two_factor" | "failed" | "disconnected";
  error: AppError | null;
}

const STATE_EVENT = "pierlink://connection-state";

/**
 * 用内置 SSH 交互登录一台服务器（按需弹密码框、验证码框），登录成功后后台功能都用这条连接。
 * @param serverId 服务器 ID
 * @returns 登录成功时完成；取消或失败时抛出后端错误
 */
export async function loginServer(serverId: string): Promise<void> {
  if (!inTauri) {
    await new Promise((r) => setTimeout(r, 300));
    return;
  }
  const { commands } = await import("../../bindings/commands");
  const r = await commands.serverLogin(serverId);
  if (r.status === "error") throw r.error;
}

/**
 * 订阅内置 SSH 连接状态
 * @param fn 每收到一条状态时调用
 * @returns 取消订阅的函数
 */
export function onConnectionState(fn: (e: ConnectionStateEvent) => void): () => void {
  if (!inTauri) return () => {};
  let unlisten: (() => void) | null = null;
  let alive = true;
  void import("@tauri-apps/api/event")
    .then(({ listen }) => listen<ConnectionStateEvent>(STATE_EVENT, (e) => fn(e.payload)))
    .then((u) => { if (alive) unlisten = u; else u(); });
  return () => { alive = false; unlisten?.(); };
}

/**
 * 是不是“内置 SSH 的服务器要验证码、后台功能自己登不上”：这时界面提供“登录”，登录后自动继续
 * @param err 后端错误
 */
export function needsLogin(err: unknown): boolean {
  const e = err as AppError | null;
  return !!e && e.code === "ssh.two_factor_required" && e.params?.engine === "builtin";
}
