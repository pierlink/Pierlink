/** 终端会话的打开方式 */
export type OpenSpec =
  | { kind: "local"; shellId: string }
  | { kind: "quick"; host: string; port: number; user: string | null }
  /** 已保存的服务器（使用它的私钥、登录 shell、保存的密码等设置） */
  | { kind: "server"; serverId: string };

/** 远程连接的目标（快速连接或已保存的服务器） */
export type RemoteSpec = Exclude<OpenSpec, { kind: "local" }>;

/** 连接服务器的一步：解析地址、建立连接、认证（握手和登录）、打开会话；ready 表示会话可以用了 */
export type ConnectStage = "resolve" | "connect" | "auth" | "session" | "ready";

/** 后端推送给终端的事件 */
export type TerminalEvent =
  | { kind: "output"; data: Uint8Array }
  /** 连接走到了哪一步（只有服务器终端有；可能跳过前面的，不会倒退） */
  | { kind: "stage"; stage: ConnectStage }
  /**
   * auth：远程终端因为密码没登录上时的原因（ssh.auth_failed 输的密码不对 / ssh.password_needed 没输或等太久 /
   * ssh.two_factor_required），其他退出为 null。
   * hooked：这次会话是带着命令预测的提示符钩子启动的（由后端决定：Windows 服务器、配置了 RemoteCommand 的服务器不装）
   */
  | { kind: "exited"; code: number | null; closedByUser: boolean; auth?: string | null; hooked?: boolean };

/** 主机密钥确认的一步：已信任，或需要用户核对这一级的指纹 */
export type HostKeyStep =
  | { kind: "trusted" }
  | { kind: "confirm"; probeId: string; host: string; level: number; keyType: string; fingerprint: string };

/** 终端后端：Tauri 实现和浏览器预览实现都遵守这个接口 */
export interface TerminalBackend {
  /** 是否是浏览器里的预览实现 */
  readonly preview: boolean;
  /** prediction：装上命令预测的 shell 钩子（服务器 bash、zsh 和本机 Git Bash；其他终端忽略） */
  open(spec: OpenSpec, cols: number, rows: number, onEvent: (e: TerminalEvent) => void, prediction?: boolean): Promise<string>;
  write(id: string, data: string): Promise<void>;
  resize(id: string, cols: number, rows: number): Promise<void>;
  /** 告诉后端前端已经处理完多少字节（流量控制） */
  ack(id: string, bytes: number): Promise<void>;
  close(id: string): Promise<void>;
  /** 终端里是否有程序在运行（shell 在等输入时为 false）；查不到时为 null */
  busy(id: string): Promise<boolean | null>;
  /** 连接前检查主机密钥；首次连接返回待确认的指纹 */
  checkHostKey(target: RemoteSpec): Promise<HostKeyStep>;
  /** 回答一级指纹确认，返回下一级或已信任 */
  answerHostKey(probeId: string, accept: boolean): Promise<HostKeyStep>;
  /** 放弃确认，结束后台的 ssh 进程 */
  cancelHostKey(probeId: string): Promise<void>;
  /**
   * 剪贴板里的图片：服务器终端传到那台服务器的 /tmp，本机终端存到本机；返回路径（本机是 Windows 路径）。
   * 剪贴板里没有图片时返回 null
   */
  pasteImage(spec: OpenSpec): Promise<string | null>;
}

/**
 * 窗格里连接服务器的前半段（核对指纹）。start、stepStart 是开始连接、当前一步开始的时间（performance.now()）。
 * phase：probe 正在核对指纹；confirm 等用户确认指纹；failed 没连上（error 是原因，failedAt 是停在哪一步）；
 * go 指纹已通过，终端接着登录、启动
 */
export interface Connecting {
  start: number;
  stepStart: number;
  phase: "probe" | "confirm" | "failed" | "go";
  error?: string;
  failedAt?: "connect" | "verify";
}
