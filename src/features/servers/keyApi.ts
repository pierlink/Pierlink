import { inTauri } from "../shell/TitleBar";

/** 服务器 authorized_keys 里的一行（只列出与 Pierlink 有关的：本电脑的、或注释以 pierlink- 开头的） */
export interface AuthorizedKeyView {
  lineNo: number;
  keyType: string;
  fingerprint: string;
  comment: string;
  /** “类型 公钥数据”，移除时原样传回 */
  publicLine: string;
  /** 与本电脑的专用密钥内容一致 */
  ours: boolean;
}

/** 一台服务器上的专用密钥状态 */
export interface KeyStatus {
  /** 本电脑的专用密钥（还没生成时为 null） */
  localFingerprint: string | null;
  keys: AuthorizedKeyView[];
  /** ~/.ssh 或 authorized_keys 权限过宽等提示（后端错误对象，按错误码翻译） */
  warnings: unknown[];
}

export interface KeyInstallResult {
  outcome: "installed" | "alreadyPresent";
  /** 服务器 ~/.ssh 或 authorized_keys 权限过宽（没有擅自修改） */
  permissionsWarning: boolean;
  /** 服务器的授权文件不在默认位置（已按验证结果确认可用） */
  nonDefaultAuthorizedKeys: boolean;
  /** 本电脑的专用密钥这次被重新生成 */
  regenerated: boolean;
  /** 需要重新安装专用密钥的其他服务器 */
  affectedServers: string[];
}

export interface KeyApi {
  /** 读取服务器上的专用密钥（需要能连上服务器：已装密钥或可用其他方式登录） */
  status(serverId: string): Promise<KeyStatus>;
  /** 用一次密码安装本电脑的专用密钥并验证；成功后这台服务器改用专用密钥登录 */
  install(serverId: string, password: string): Promise<KeyInstallResult>;
  /** 从服务器移除指定的一行（只删内容完全一致的行）；authorized_keys 是符号链接时返回 "symlink" */
  remove(serverId: string, publicLine: string): Promise<"removed" | "notFound" | "symlink">;
}

/** 浏览器预览：示例数据 */
function createPreviewKeyApi(): KeyApi {
  let installed = false;
  const ours = { lineNo: 3, keyType: "ssh-ed25519", fingerprint: "SHA256:q8Fz0mX3lV7nB2cR6tY1uI9oP4aS5dD8fG0hJ2kL3zA", comment: "pierlink-DESKTOP-01-demo0001", publicLine: "ssh-ed25519 AAAAours", ours: true };
  let others: AuthorizedKeyView[] = [
    { lineNo: 5, keyType: "ssh-ed25519", fingerprint: "SHA256:Wm4vN8bQ1xC7zL0kJ6hG3fD9sA2pO5iU8yT1rE4wQ7e", comment: "pierlink-LAPTOP-old-a1b2c3d4", publicLine: "ssh-ed25519 AAAAold", ours: false },
  ];
  return {
    async status() {
      await new Promise((r) => setTimeout(r, 300));
      return { localFingerprint: ours.fingerprint, keys: [...(installed ? [ours] : []), ...others], warnings: [] };
    },
    async install(_id, password) {
      await new Promise((r) => setTimeout(r, 800));
      if (password !== "demo") throw { code: "ssh.auth_failed", params: {} };
      installed = true;
      return { outcome: "installed", permissionsWarning: false, nonDefaultAuthorizedKeys: false, regenerated: false, affectedServers: [] };
    },
    async remove(_id, line) {
      if (line === ours.publicLine) installed = false;
      others = others.filter((k) => k.publicLine !== line);
      return "removed";
    },
  };
}

let api: Promise<KeyApi> | null = null;

/** 取得专用密钥接口：应用里用真实后端，浏览器预览用示例数据 */
export function getKeyApi(): Promise<KeyApi> {
  api ??= inTauri ? import("./keyTauri").then((m) => m.createTauriKeyApi()) : Promise.resolve(createPreviewKeyApi());
  return api;
}
