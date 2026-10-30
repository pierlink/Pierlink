import type { RemoteSpec } from "../terminal/types";

/** 文件所在的机器：本机，或某个远程目标（已保存的服务器 / 快速连接） */
export type Loc = { kind: "local" } | RemoteSpec;

export type EntryKind = "file" | "dir" | "symlink" | "other";

/** 目录里的一项 */
export interface FileEntry {
  name: string;
  kind: EntryKind;
  size: number;
  /** 修改时间（秒） */
  mtime: number;
  /** 隐藏文件（以 . 开头，或 Windows 隐藏属性） */
  hidden: boolean;
  /** 符号链接指向的是不是目录（未知为 null） */
  targetIsDir: boolean | null;
}

/** 列目录的结果 */
export interface Listing {
  /** 实际列出的目录（服务器上是解析后的绝对路径） */
  path: string;
  entries: FileEntry[];
  /** 条目太多，只列出了前面一部分 */
  truncated: boolean;
}

/** 一次传输：把 from 上的若干路径传到 to 的 destDir 目录里 */
export interface TransferRequest {
  from: Loc;
  sources: string[];
  to: Loc;
  destDir: string;
}

/** 传输前的统计：文件数、总字节、目标里已存在的同名项 */
export interface TransferPlanView {
  files: number;
  bytes: number;
  conflicts: string[];
}

export type ConflictChoice = "error" | "overwrite" | "skip";

export interface TransferProgressView {
  totalBytes: number;
  doneBytes: number;
  current: string;
  filesDone: number;
  filesTotal: number;
}

export interface TransferReportView {
  done: number;
  skipped: string[];
  /** 下载到 Windows 时改过名的项（原名里有 Windows 不允许的字符） */
  renamed: { from: string; to: string }[];
  failed: { path: string; reason: string }[];
}

/** 文件版本：大小和修改时间（秒）。保存前用它检查文件是否被别人改过 */
export interface FileVersion {
  size: number;
  mtime: number;
  /** 内容的 SHA-256；同一秒内、大小不变的修改靠它发现 */
  hash: string | null;
}

/** 读出来的文本文件 */
export interface TextFileView {
  text: string;
  /** 原文件开头有 UTF-8 BOM（保存时照样写回） */
  bom: boolean;
  version: FileVersion;
}
