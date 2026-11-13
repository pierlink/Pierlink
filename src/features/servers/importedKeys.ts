import { inTauri } from "../shell/TitleBar";

/** 一把导入的私钥（加密存在 Pierlink 里；服务器的“指定私钥”从这里选，identityFile 填 path） */
export interface ImportedKey {
  id: string;
  name: string;
  /** 算法，如 ssh-ed25519、ssh-rsa */
  algorithm: string;
  /** 公钥指纹（SHA256:…） */
  fingerprint: string;
  comment: string;
  /** 原文件有密码（用户自己的密码）；没有的话密码是随机生成、一定存着 */
  userPassphrase: boolean;
  /** 密码存在凭据管理器里，连接时自动填 */
  passphraseSaved: boolean;
  /** 存着的私钥的完整路径 */
  path: string;
  /** 用这把私钥的服务器名 */
  usedBy: string[];
}

export interface ImportedKeysApi {
  list(): Promise<ImportedKey[]>;
  /** 导入：原文件有密码时要给密码；savePassphrase 时把密码存进凭据管理器 */
  import(path: string, name: string, passphrase: string | null, savePassphrase: boolean): Promise<ImportedKey>;
  rename(id: string, name: string): Promise<ImportedKey>;
  /** 删除；还有服务器在用时报 keys.imported_in_use */
  remove(id: string): Promise<void>;
  /** 保存（核对过能解开）或忘掉（null）用户自己设的密码 */
  passphrase(id: string, passphrase: string | null): Promise<void>;
  /** 系统的“打开文件”框；取消时为 null */
  pickFile(): Promise<string | null>;
}

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

async function tauriApi(): Promise<ImportedKeysApi> {
  const { commands } = await import("../../bindings/commands");
  return {
    async list() { return unwrap(await commands.keysImportedList()); },
    async import(path, name, passphrase, save) { return unwrap(await commands.keysImport(path, name, passphrase, save)); },
    async rename(id, name) { return unwrap(await commands.keysImportedRename(id, name)); },
    async remove(id) { unwrap(await commands.keysImportedRemove(id)); },
    async passphrase(id, passphrase) { unwrap(await commands.keysImportedPassphrase(id, passphrase)); },
    async pickFile() { return unwrap(await commands.keysPickFile()); },
  };
}

/** 浏览器预览：内存里的示例私钥。文件名里带 enc 的当作有密码（密码是 test） */
function previewApi(): ImportedKeysApi {
  let keys: ImportedKey[] = [{
    id: "a1b2c3d4e5f60718", name: "id_ed25519", algorithm: "ssh-ed25519", fingerprint: "SHA256:k3Qx0q1mW8vC9ZpY2nF7rT4uH6sJ5dL3aB1cE0gI8oQ",
    comment: "dev@laptop", userPassphrase: false, passphraseSaved: true, path: "C:\\Users\\dev\\AppData\\Roaming\\Pierlink\\keys\\imported\\a1b2c3d4e5f60718.key", usedBy: ["gpu-dev"],
  }];
  const err = (code: string, params: Record<string, string> = {}) => ({ code, params });
  return {
    async list() { return keys.map((k) => ({ ...k })); },
    async import(path, name, passphrase, save) {
      await new Promise((r) => setTimeout(r, 300));
      const file = path.split(/[\\/]/).pop() ?? "";
      if (!file) throw err("keys.imported_unreadable", { path });
      if (file.endsWith(".pub")) throw err("keys.imported_public_key");
      const encrypted = file.includes("enc");
      if (encrypted && !passphrase) throw err("keys.imported_needs_passphrase");
      if (encrypted && passphrase !== "test") throw err("keys.imported_wrong_passphrase");
      // 导入过的同一把私钥直接用原来那一把
      const same = keys.find((k) => k.name === file);
      if (same) return { ...same };
      const id = Math.random().toString(16).slice(2, 18).padEnd(16, "0");
      const key: ImportedKey = {
        id, name: name.trim() || file, algorithm: "ssh-ed25519", fingerprint: `SHA256:${id}preview`, comment: "", userPassphrase: encrypted,
        passphraseSaved: !encrypted || save, path: `C:\\Users\\dev\\AppData\\Roaming\\Pierlink\\keys\\imported\\${id}.key`, usedBy: [],
      };
      keys = [...keys, key];
      return { ...key };
    },
    async rename(id, name) {
      if (!name.trim()) throw err("keys.imported_name_invalid");
      keys = keys.map((k) => (k.id === id ? { ...k, name: name.trim() } : k));
      const key = keys.find((k) => k.id === id);
      if (!key) throw err("keys.imported_not_found");
      return { ...key };
    },
    async remove(id) {
      const key = keys.find((k) => k.id === id);
      if (key?.usedBy.length) throw err("keys.imported_in_use", { servers: key.usedBy.join("、") });
      keys = keys.filter((k) => k.id !== id);
    },
    async passphrase(id, passphrase) {
      if (passphrase !== null && passphrase !== "test") throw err("keys.imported_wrong_passphrase");
      keys = keys.map((k) => (k.id === id ? { ...k, passphraseSaved: passphrase !== null } : k));
    },
    async pickFile() { return "C:\\Users\\dev\\.ssh\\id_rsa_enc"; },
  };
}

const KEYS_CHANGED = "pierlink:imported-keys-changed";

/**
 * 订阅“导入的私钥变了”（导入、改名、删除、保存或忘掉密码）
 * @param fn 收到通知时调用
 * @returns 取消订阅的函数
 */
export function onKeysChanged(fn: () => void): () => void {
  window.addEventListener(KEYS_CHANGED, fn);
  return () => window.removeEventListener(KEYS_CHANGED, fn);
}

/** 改动成功后通知一声（同一个接口对象上的改动都会通知） */
function withNotify(api: ImportedKeysApi): ImportedKeysApi {
  const after = async <T>(p: Promise<T>): Promise<T> => { const r = await p; window.dispatchEvent(new Event(KEYS_CHANGED)); return r; };
  return {
    ...api,
    import: (path, name, passphrase, save) => after(api.import(path, name, passphrase, save)),
    rename: (id, name) => after(api.rename(id, name)),
    remove: (id) => after(api.remove(id)),
    passphrase: (id, passphrase) => after(api.passphrase(id, passphrase)),
  };
}

let api: Promise<ImportedKeysApi> | null = null;

/** 取得导入私钥的接口（在 Tauri 窗口里用真实后端，浏览器预览用示例） */
export function getImportedKeysApi(): Promise<ImportedKeysApi> {
  api ??= (inTauri ? tauriApi() : Promise.resolve(previewApi())).then(withNotify);
  return api;
}

/** 下拉框、列表里显示的一行：名字 · 算法 · 指纹前几位 */
export function keyLabel(k: ImportedKey): string {
  const algo = k.algorithm.replace(/^ssh-/, "").replace(/^ecdsa-sha2-/, "ecdsa-");
  return `${k.name} · ${algo} · ${k.fingerprint.replace(/^SHA256:/, "").slice(0, 8)}`;
}
