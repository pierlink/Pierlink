import { inTauri } from "../shell/TitleBar";
import type { ConflictChoice, FileVersion, Listing, Loc, TextFileView, TransferPlanView, TransferProgressView, TransferReportView, TransferRequest } from "./types";

/** 文件面板用到的后端接口：Tauri 实现和浏览器预览实现都遵守它 */
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
  /** 重命名（目标已存在时报错） */
  rename(loc: Loc, from: string, to: string): Promise<void>;
  createDir(loc: Loc, path: string): Promise<void>;
  /** 新建空文件（已存在时报错） */
  createFile(loc: Loc, path: string): Promise<void>;
  /** 删除：本机放进回收站，服务器上直接删除；返回删除的项数 */
  remove(loc: Loc, paths: string[]): Promise<number>;
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
