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
  /**
   * 程序自己结束了。hookHint：带着命令预测的提示符钩子连上后很快就退出、一个提示符都没出现，
   * 多半是钩子和这台服务器不兼容：off 表示已自动为这台服务器关掉预测；quick 表示快速连接，提示到设置里关
   */
  | { kind: "exited"; code: number | null; hookHint?: "off" | "quick" }
  /** 用户点了“断开” */
  | { kind: "disconnected" }
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

/**
 * 断开、退出这几种不盖提示块：终端内容留着，只在最下面写一行说明。
 * 登录失败、没能打开等要用户选择怎么办的，仍然盖上提示块。
 * @param ended 结束的情况
 * @returns true 表示写一行说明，不盖提示块
 */
export function endsInline(ended: Ended): boolean {
  return ended.kind === "lost" || ended.kind === "exited" || ended.kind === "disconnected";
}

/**
 * 结束时写进终端的内容：先退出全屏程序的备用屏幕，恢复光标、按键模式、滚动区域，关掉鼠标上报，
 * 再在新的一行写上说明，下面空出两行给右下角的按钮，最后隐藏光标（会话已经结束，不能再输入）。
 * @param text 说明文字
 * @param warn true 用黄色（连接断开），false 用灰色
 * @param alt 当前是不是在备用屏幕（vim、less 等全屏程序里）
 * @returns 要写进终端的文字
 */
export function endedLine(text: string, warn: boolean, alt: boolean): string {
  // \x1b[!p 是软复位：光标显示、键盘模式、滚动区域、字符属性回到默认，不移动光标
  const reset = `${alt ? "\x1b[?1049l" : ""}\x1b[!p\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l`;
  // 说明下面多空两行：右下角的按钮条约占三行高，不挡住说明
  return `${reset}\r\n\x1b[${warn ? "33" : "90"}m${text}\x1b[0m\r\n\r\n\r\n\x1b[?25l`;
}
