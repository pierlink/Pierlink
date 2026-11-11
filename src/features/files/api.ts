import { inTauri } from "../shell/TitleBar";
import type { ConflictChoice, DragOutLabels, DragOutResult, FileVersion, Listing, Loc, TextFileView, TransferPlanView, TransferProgressView, TransferReportView, TransferRequest } from "./types";

/** 文件面板用到的后端接口：Tauri 实现和浏览器预览实现都遵守它 */
/** 删除进度：已经删掉几项、正在删哪个 */
export interface DeleteProgress {
  deleted: number;
  current: string;
}

export interface FilesApi {
  readonly preview: boolean;
  /** 家目录（本机为用户目录，服务器为登录后的初始目录） */
  home(loc: Loc): Promise<string>;
  /** 列目录（含隐藏文件）；条目太多时截断 */
  list(loc: Loc, path: string): Promise<Listing>;
  /**
   * 服务器终端当前所在目录（按终端会话查询）。
   * @returns 目录；服务器不支持（没有 /proc、不接受环境变量）或找不到时为 null
   */
  terminalCwd(sessionId: string): Promise<string | null>;
  /** 传输前统计：文件数、总大小、目标里已经存在的同名项 */
  plan(req: TransferRequest): Promise<TransferPlanView>;
  /** 开始传输；返回传输 ID（用于取消）和完成时的报告 */
  transfer(req: TransferRequest, conflict: ConflictChoice, onProgress: (p: TransferProgressView) => void): { id: Promise<string>; done: Promise<TransferReportView> };
  cancel(transferId: string): Promise<void>;
  /**
   * 把服务器上的文件拖到窗口外（资源管理器、桌面等）：开始系统拖放，松手放到别的程序上时才下载。
   * 开始下载时 onProgress 先收到一条全是 0 的进度。
   * @returns 传输 ID（取消用）和拖放结束时的结果；下载出错时 done 抛出错误（取消是 sftp.cancelled）
   */
  dragOut(from: Loc, sources: string[], labels: DragOutLabels, onProgress: (p: TransferProgressView) => void): { id: string; done: Promise<DragOutResult> };
  /** 重命名（目标已存在时报错） */
  rename(loc: Loc, from: string, to: string): Promise<void>;
  createDir(loc: Loc, path: string): Promise<void>;
  /** 新建空文件（已存在时报错） */
  createFile(loc: Loc, path: string): Promise<void>;
  /**
   * 删除：本机放进回收站；服务器上逐项删除，onProgress 收到已删几项、正在删哪个，可以用 cancel(id) 停下（done 抛出 sftp.cancelled）；
   * fast 时服务器上用 rm -rf 一次删完（没有进度，不能停）。
   * @returns 任务 ID（停下用）和删完时的结果（删除的项数）
   */
  remove(loc: Loc, paths: string[], fast: boolean, onProgress: (p: DeleteProgress) => void): { id: string; done: Promise<number> };
  /** 读文本文件（软件内编辑）；太大或不是文本时报错 */
  readText(loc: Loc, path: string): Promise<TextFileView>;
  /** 保存文本文件；expected 为打开时的版本，文件之后被改过时报 files.changed（传 null 直接覆盖） */
  writeText(loc: Loc, path: string, text: string, bom: boolean, expected: FileVersion | null): Promise<FileVersion>;
  /** 服务器文件的权限位；本机为 null */
  mode(loc: Loc, path: string): Promise<number | null>;
  chmod(loc: Loc, path: string, mode: number): Promise<void>;
  /** 用系统默认程序打开本机文件或文件夹 */
  openDefault(path: string): Promise<void>;
  /** 在资源管理器中显示本机文件 */
  reveal(path: string): Promise<void>;
}

let api: Promise<FilesApi> | null = null;

/** 取得文件接口（在 Tauri 窗口里用真实后端，浏览器预览用假数据） */
export function getFilesApi(): Promise<FilesApi> {
  api ??= inTauri
    ? import("./tauriApi").then((m) => m.createTauriFilesApi())
    : import("./previewApi").then((m) => m.createPreviewFilesApi());
  return api;
}
