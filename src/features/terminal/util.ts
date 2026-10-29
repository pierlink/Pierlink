import type { OpenSpec } from "./types";

/**
 * 解析快速连接输入：`用户@主机`、`主机:端口`、`用户@主机:端口`，IPv6 用方括号。
 * @param input 用户输入
 * @returns 解析结果；格式不对时返回 null
 */
export function parseQuickTarget(input: string): Extract<OpenSpec, { kind: "quick" }> | null {
  const s = input.trim();
  if (!s || /\s/.test(s) || s.startsWith("-")) return null;
  const at = s.lastIndexOf("@");
  const user = at > 0 ? s.slice(0, at) : null;
  const rest = at > 0 ? s.slice(at + 1) : s;
  let host = rest;
  let port = 22;
  const v6 = /^\[([^\]]+)\](?::(\d+))?$/.exec(rest);
  if (v6) {
    host = v6[1];
    if (v6[2]) port = Number(v6[2]);
  } else if (rest.split(":").length === 2) {
    const [h, p] = rest.split(":");
    host = h;
    port = Number(p);
  } else if (rest.includes(":")) {
    return null;
  }
  if (!host || !Number.isInteger(port) || port < 1 || port > 65535) return null;
  return { kind: "quick", host, port, user };
}

/**
 * 是否需要在粘贴前确认：多行文本，并且远程程序没有开启括号粘贴模式（异常 R2）。
 * @param text 要粘贴的文本
 * @param bracketed 终端当前是否处于括号粘贴模式
 * @param broadcast 是否处于广播输入模式（广播时多行一律确认）
 */
export function needsPasteConfirm(text: string, bracketed: boolean, broadcast = false): boolean {
  const multiLine = /[\r\n]/.test(text.replace(/[\r\n]+$/, ""));
  return multiLine && (broadcast || !bracketed);
}

/** base64 解码为字节（后端输出按 base64 传输，避免多字节字符被拆开） */
export function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/**
 * 解析 OSC 7 当前目录报告：`file://主机/路径`（路径经 URL 编码）。
 * Windows 路径形如 `file://主机/C:/Users/me`，转成 `C:\Users\me`。
 * @param data OSC 7 的内容（不含 `7;`）
 * @returns 路径；格式不对返回 null
 */
export function parseOsc7(data: string): string | null {
  const m = /^file:\/\/[^/]*(\/.*)$/.exec(data.trim());
  if (!m) return null;
  let path: string;
  try { path = decodeURIComponent(m[1]); } catch { return null; }
  if (/^\/[A-Za-z]:([/\\]|$)/.test(path)) return path.slice(1).replace(/\//g, "\\") || null;
  return path;
}

/**
 * 解析 OSC 9;9 当前目录报告（Windows Terminal 约定）：`"C:\路径"`，引号可有可无。
 * @param data `9;` 之后的内容
 * @returns 路径；为空返回 null
 */
export function parseOsc99(data: string): string | null {
  let s = data.trim();
  if (s.length >= 2 && s.startsWith('"') && s.endsWith('"')) s = s.slice(1, -1);
  return s || null;
}
