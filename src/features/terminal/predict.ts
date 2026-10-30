import type { OpenSpec } from "./types";
import { shellStyle } from "./localPaths";

/** 一条执行过的命令（来自 shell 钩子的记录） */
export interface HistoryEntry {
  command: string;
  /** 敲这条命令时所在的目录；不知道时为 null */
  cwd: string | null;
  /** 退出码；不知道时为 null */
  exit: number | null;
  /** 执行时间（毫秒） */
  at: number;
}

/** 单条命令最长记多少个字符：再长的多半是粘贴的脚本，不值得预测 */
export const MAX_COMMAND = 1000;

/**
 * 命令历史按什么分开存：每台保存的服务器一份，快速连接按“用户@主机:端口”，本机 Git Bash 一份。
 * @param spec 终端的打开参数
 * @returns 历史的范围键；不做预测的终端（PowerShell、命令提示符、WSL 等）为 null
 */
export function historyScope(spec: OpenSpec): string | null {
  if (spec.kind === "server") return `server:${spec.serverId}`;
  if (spec.kind === "quick") return `quick:${spec.user ?? ""}@${spec.host.toLowerCase()}:${spec.port}`;
  return shellStyle(spec.shellId).kind === "msys" ? "local:bash" : null;
}

/** 退出码算不算失败：Ctrl+C（130）和 SIGTERM（143）结束的不算，常驻的开发服务器、训练脚本多半这样结束 */
function failed(exit: number | null): boolean {
  return exit !== null && exit !== 0 && exit !== 130 && exit !== 143;
}

/**
 * 按已输入的内容挑一条预测：只从以它开头、更长、没失败过的命令里挑；同一目录下用过的优先，其次最近用过的。
 * @param entries 这个范围的历史（按时间先后）
 * @param input 已输入的内容
 * @param cwd 当前目录；不知道时为 null
 * @returns 整条预测命令；没有合适的为 null
 */
export function suggest(entries: readonly HistoryEntry[], input: string, cwd: string | null): string | null {
  if (!input.trim() || input.includes("\n")) return null;
  let anywhere: string | null = null;
  for (let i = entries.length - 1; i >= 0; i--) {
    const e = entries[i];
    if (failed(e.exit) || e.command.length <= input.length || !e.command.startsWith(input)) continue;
    if (cwd !== null && e.cwd === cwd) return e.command;
    anywhere ??= e.command;
    if (cwd === null) break;
  }
  return anywhere;
}

/**
 * Ctrl+→ 补一个词：预测剩下的部分里，到下一个词结尾（路径按 / 分段）为止。
 * @param rest 预测里还没输入的部分
 * @returns 这次要补上的文字
 */
export function nextWord(rest: string): string {
  const m = /^\s*[^\s/]*\/?/.exec(rest);
  return m && m[0] ? m[0] : rest;
}

const SENSITIVE = [
  /passw(or)?d|passwd|secret|token|api[_-]?key|private[_-]?key|BEGIN [A-Z ]*PRIVATE KEY|authorization|sshpass|--password/i,
  /(^|\s)-p\S+/,
  /AKIA[0-9A-Z]{16}/,
  /\b(ghp_|github_pat_|xox[abprs]-|sk-)[A-Za-z0-9_-]{20,}/,
];

/**
 * 这条命令能不能记：可能带口令、令牌、私钥的不记；含控制字符或太长的也不记。宁可少记，不能把秘密存下来。
 * @param command 命令原文
 * @returns 不能记时为 true
 */
export function isSensitive(command: string): boolean {
  if (command.length > MAX_COMMAND || /[\u0000-\u001f\u007f]/.test(command)) return true;
  return SENSITIVE.some((r) => r.test(command));
}
