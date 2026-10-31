import { inTauri } from "../shell/TitleBar";

/** ssh config 里的一个可导入的别名（参数来自 `ssh -G`，只读显示） */
export interface ImportCandidate {
  alias: string;
  /** 定义它的文件和行号 */
  file: string;
  line: number;
  hostname: string | null;
  user: string | null;
  port: number | null;
  identityFiles: string[];
  /** 经跳板机（ProxyJump / ProxyCommand） */
  viaJump: boolean;
  /** 配置了端口转发（后台连接会自动清除，S21） */
  hasForwards: boolean;
  /** 关闭了主机密钥校验（S12） */
  hostKeyCheckWeakened: boolean;
  alreadyImported: boolean;
  /** 这个别名解析失败（后端错误对象） */
  error: unknown | null;
}

export interface ImportSkipped {
  pattern: string;
  reason: "wildcard" | "negated" | "inMatchBlock" | "invalidAlias" | "duplicate";
}

export interface ImportPreview {
  configPath: string;
  candidates: ImportCandidate[];
  skipped: ImportSkipped[];
  /** 读某些被包含文件失败等（后端错误对象） */
  warnings: unknown[];
}

export interface ImportApi {
  /** 读取 ssh config 并逐个解析别名；可被 cancel 打断 */
  preview(): Promise<ImportPreview>;
  cancel(): Promise<void>;
  /** 导入选中的别名，返回导入失败的别名及原因 */
  importAliases(aliases: string[], groupName: string | null): Promise<{ alias: string; error: unknown }[]>;
}

/** 浏览器预览：示例配置 */
function createPreviewImportApi(): ImportApi {
  const base = { file: "C:\\Users\\dev\\.ssh\\config", identityFiles: [], viaJump: false, hasForwards: false, hostKeyCheckWeakened: false, alreadyImported: false, error: null };
  return {
    async preview() {
      await new Promise((r) => setTimeout(r, 300));
      return {
        configPath: "C:\\Users\\dev\\.ssh\\config",
        candidates: [
          { ...base, alias: "gpu-a100", line: 1, hostname: "203.0.113.21", user: "ml", port: 22, identityFiles: ["~/.ssh/id_ed25519"] },
          { ...base, alias: "web-prod", line: 7, hostname: "198.51.100.30", user: "deploy", port: 2222, viaJump: true },
          { ...base, alias: "db-tunnel", line: 14, hostname: "203.0.113.5", user: "admin", port: 22, hasForwards: true },
          { ...base, alias: "lab-old", line: 21, hostname: "192.0.2.50", user: "root", port: 22, hostKeyCheckWeakened: true },
          { ...base, alias: "gpu-dev", line: 30, hostname: "203.0.113.18", user: "dev", port: 22, alreadyImported: true },
          { ...base, alias: "broken", line: 36, hostname: null, user: null, port: null, error: { code: "ssh.resolve_failed", params: {} } },
        ],
        skipped: [
          { pattern: "*", reason: "wildcard" },
          { pattern: "*.internal", reason: "wildcard" },
          { pattern: "!bastion", reason: "negated" },
          { pattern: "Match host *.corp", reason: "inMatchBlock" },
        ],
        warnings: [],
      };
    },
    async cancel() {},
    async importAliases() { return []; },
  };
}

let api: Promise<ImportApi> | null = null;

/** 取得导入接口：应用里用真实后端，浏览器预览用示例数据 */
export function getImportApi(): Promise<ImportApi> {
  api ??= inTauri ? import("./importTauri").then((m) => m.createTauriImportApi()) : Promise.resolve(createPreviewImportApi());
  return api;
}
