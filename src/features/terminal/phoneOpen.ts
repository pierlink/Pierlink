/**
 * 手机请电脑打开终端：电脑界面收到请求后按正常流程打开（指纹、密码照常在电脑上处理），
 * 拿到会话 ID 或失败时回报给后端，后端再回给手机。只在桌面程序里有，浏览器预览时不调用。
 */
import { formatError, isAppError, type AppError } from "../../errors";

/** 手机要打开的终端：serverId 为 null 时是本机终端 */
export interface PhoneOpenRequest {
  id: number;
  serverId: string | null;
}

const OPEN_EVENT = "pierlink://mobile-open-terminal";

/**
 * 监听手机的打开请求。
 * @param cb 收到请求时调用
 * @returns 取消监听的函数
 */
export async function onPhoneOpen(cb: (r: PhoneOpenRequest) => void): Promise<() => void> {
  const { listen } = await import("@tauri-apps/api/event");
  return listen<PhoneOpenRequest>(OPEN_EVENT, (e) => cb(e.payload));
}

/**
 * 回报打开结果。
 * @param id 请求编号
 * @param session 打开了的会话 ID；没打开时为 null
 * @param error 没打开的原因
 */
export async function reportPhoneOpen(id: number, session: string | null, error: unknown): Promise<void> {
  const { commands } = await import("../../bindings/commands");
  // 带上按电脑界面语言写好的说明（手机上没有这些错误码的译文）
  const reason: AppError | null = session ? null
    : isAppError(error) ? { code: error.code, params: { ...error.params, message: error.params.message ?? formatError(error) } }
    : { code: "mobile.open_failed", params: { message: String(error ?? "") } };
  const r = await commands.mobileTerminalOpened(id, session, reason);
  if (r.status === "error") throw r.error;
}
