/** 批量添加：每行一台服务器，格式 `用户@地址[:端口] [名称] [分组]`，用空白分隔；# 开头为注释 */

export interface BatchLine {
  /** 原文行号（从 1 开始） */
  line: number;
  text: string;
  /** 解析成功的服务器；失败时为 null */
  server: { name: string; host: string; port: number; username: string | null; groupName: string | null } | null;
  /** 失败原因（翻译键） */
  error: "format" | "port" | "duplicate" | null;
}

const TARGET = /^(?:([^@\s]+)@)?(\[[^\]\s]+\]|[^:\s@]+)(?::(\d{1,5}))?$/;

/**
 * 解析批量添加的文本。
 * @param text 用户粘贴的多行文本
 * @param existing 已有服务器的“用户@地址:端口”，重复的行标为 duplicate
 * @returns 每个非空、非注释行的解析结果
 */
export function parseBatch(text: string, existing: Iterable<string> = []): BatchLine[] {
  const seen = new Set(existing);
  const out: BatchLine[] = [];
  text.split(/\r?\n/).forEach((raw, i) => {
    const trimmed = raw.trim();
    if (!trimmed || trimmed.startsWith("#")) return;
    const [target, name, ...rest] = trimmed.split(/\s+/);
    const m = TARGET.exec(target);
    if (!m) { out.push({ line: i + 1, text: trimmed, server: null, error: "format" }); return; }
    const port = m[3] ? Number(m[3]) : 22;
    if (!Number.isInteger(port) || port < 1 || port > 65535) { out.push({ line: i + 1, text: trimmed, server: null, error: "port" }); return; }
    const host = m[2].replace(/^\[|\]$/g, "");
    const username = m[1] ?? null;
    const key = targetKey(username, host, port);
    if (seen.has(key)) { out.push({ line: i + 1, text: trimmed, server: null, error: "duplicate" }); return; }
    seen.add(key);
    const groupName = rest.length ? rest.join(" ") : null;
    out.push({ line: i + 1, text: trimmed, server: { name: name ?? host, host, port, username, groupName }, error: null });
  });
  return out;
}

/** 判断重复用的键：用户@地址:端口（地址不区分大小写） */
export function targetKey(username: string | null, host: string, port: number): string {
  return `${username ?? ""}@${host.toLowerCase()}:${port}`;
}
