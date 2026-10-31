import { inTauri } from "../shell/TitleBar";

/**
 * ssh 要密码（或其他回答）时后端推来的请求。
 * password：服务器密码（输一次，本次运行所有功能共用）；other：验证码、私钥口令、跳板机密码等；
 * confirm：是或否；notice：只需知道。
 */
export interface PasswordRequest {
  id: number;
  serverId: string;
  serverName: string;
  kind: "password" | "other" | "confirm" | "notice";
  /** 其他提示的原文（来自 ssh 或服务器，只当文字显示） */
  prompt: string | null;
  /** 发起的功能：终端 / 文件面板 … */
  purpose: string;
  /** 能否“记住密码” */
  canRemember: boolean;
}

/** 密码接口：Tauri 实现和浏览器预览实现都遵守它 */
export interface PasswordsApi {
  /** 现在等待回答的请求（页面重新加载后恢复显示） */
  pending(): Promise<PasswordRequest[]>;
  /** 回答一个请求；password 为 null 表示取消。返回 false 表示请求已经结束（超时等） */
  answer(id: number, password: string | null, remember: boolean): Promise<boolean>;
  /** 用户主动输入某台服务器的密码（后台功能“等待输入密码”时） */
  enter(serverId: string, password: string, remember: boolean): Promise<void>;
  onRequest(cb: (r: PasswordRequest) => void): Promise<() => void>;
  /** 某个请求结束了（超时、退出），关掉对应的框 */
  onClosed(cb: (id: number) => void): Promise<() => void>;
  /** 某台服务器有了新密码 */
  onSaved(cb: (serverId: string) => void): Promise<() => void>;
}

/** 浏览器预览：没有真正的后端；#password-demo 打开一个示例请求 */
function previewApi(): PasswordsApi {
  const requests = new Set<(r: PasswordRequest) => void>();
  const saved = new Set<(id: string) => void>();
  if (import.meta.env.DEV && window.location.hash === "#password-demo") {
    setTimeout(() => {
      for (const cb of requests) cb({ id: 1, serverId: "demo", serverName: "gpu-a100", kind: "password", prompt: null, purpose: "终端", canRemember: true });
    }, 300);
  }
  return {
    pending: async () => [],
    answer: async () => true,
    enter: async (serverId) => { for (const cb of saved) cb(serverId); },
    onRequest: async (cb) => { requests.add(cb); return () => { requests.delete(cb); }; },
    onClosed: async () => () => {},
    onSaved: async (cb) => { saved.add(cb); return () => { saved.delete(cb); }; },
  };
}

let api: Promise<PasswordsApi> | null = null;

/** 取当前环境的密码接口（Tauri 里用真实后端，浏览器预览用示例） */
export function getPasswordsApi(): Promise<PasswordsApi> {
  api ??= inTauri ? import("./tauriApi").then((m) => m.tauriPasswordsApi) : Promise.resolve(previewApi());
  return api;
}
