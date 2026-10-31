import type { ServerRecord } from "../servers/api";

/**
 * 能否共用和记住密码：应用内新建、填了用户名、用密码或默认密钥登录的服务器（与后端 passwords.rs 的规则一致）。
 * @param s 服务器记录
 */
export function canSharePassword(s: ServerRecord | undefined): boolean {
  return !!s && s.source === "manual" && !!s.username && !!s.host && (s.authMethod === "password" || s.authMethod === "agent");
}
