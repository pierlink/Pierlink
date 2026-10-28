import { formatError, isAppError } from "../../errors";

/** 会话结束后盖在终端上的提示块 */
export type Ended =
  /** 连接断开（网络中断、服务器重启等，ssh 以 255 退出） */
  | { kind: "lost" }
  /**
   * 登录失败（密码、密钥不对）；password：用的是 Pierlink 记着的密码（已作废，重新连接会再问）；
   * builtin：内置 SSH 登录失败（可能是密钥只写在 ~/.ssh/config 里），可以改用系统 OpenSSH；
   * tooManyAuth：ssh-agent 里的密钥太多，超过服务器允许的登录尝试次数（MaxAuthTries）被断开
   */
  | { kind: "authFailed"; password?: boolean; twoFactor?: boolean; builtin?: boolean; tooManyAuth?: boolean }
  /** 没输密码（取消了密码框，或密码框开着时 ssh 等不及先退出了）：输入密码后自动重新连接 */
  | { kind: "passwordNeeded" }
  /** 程序自己结束了 */
  | { kind: "exited"; code: number | null }
  /** 没能打开；builtin：内置 SSH 连不上，可以改用系统 OpenSSH 重试 */
  | { kind: "failed"; text: string; builtin?: boolean };

/**
 * 打开失败的错误换成提示块。内置 SSH 在打开时就连接和登录，登录类错误和系统 OpenSSH 退出时的提示保持一致；
 * 后端给内置 SSH 的失败标了 engine=builtin。
 * @param err 后端错误
 * @returns 提示块的内容
 */
export function endedFromOpenError(err: unknown): Ended {
  if (!isAppError(err)) return { kind: "failed", text: formatError(err) };
  const builtin = err.params?.engine === "builtin";
  switch (err.code) {
    case "ssh.password_needed":
    case "ssh.auth_cancelled":
      return { kind: "passwordNeeded" };
    case "ssh.auth_failed":
      // 带 password_generation 表示输的密码被服务器拒绝、已作废；否则是密钥等其他方式没通过
      return "password_generation" in (err.params ?? {}) ? { kind: "authFailed", password: true } : { kind: "authFailed", builtin };
    case "ssh.two_factor_required":
      return { kind: "authFailed", twoFactor: true };
    // 不提供改用系统 OpenSSH：agent 里密钥多、又没指定私钥时，换成系统 OpenSSH 一样撞上限
    case "ssh.too_many_auth_attempts":
      return { kind: "authFailed", tooManyAuth: true };
    // 指纹变了：改用系统 OpenSSH 也一样会拒绝，不提供改用
    case "ssh.host_key_changed":
      return { kind: "failed", text: formatError(err) };
    // 内置 SSH 不认这台服务器的指纹（例如只靠 @cert-authority 信任）：系统 OpenSSH 可能认，提供改用
    case "ssh.host_key_unknown":
      return { kind: "failed", text: formatError(err), builtin };
    default:
      return { kind: "failed", text: formatError(err), builtin };
  }
}
