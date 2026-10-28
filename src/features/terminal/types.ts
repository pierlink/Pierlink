/** 终端会话的打开方式 */
export type OpenSpec =
  | { kind: "local"; shellId: string }
  | { kind: "quick"; host: string; port: number; user: string | null }
  /** 已保存的服务器（使用它的私钥、登录 shell、保存的密码等设置） */
  | { kind: "server"; serverId: string };

/** 远程连接的目标（快速连接或已保存的服务器） */
export type RemoteSpec = Exclude<OpenSpec, { kind: "local" }>;

/** 后端推送给终端的事件 */
export type TerminalEvent =
  | { kind: "output"; data: Uint8Array }
  /**
   * auth：远程终端因为密码没登录上时的原因（ssh.auth_failed 输的密码不对 / ssh.password_needed 没输或等太久 /
   * ssh.two_factor_required），其他退出为 null
   */
  | { kind: "exited"; code: number | null; closedByUser: boolean; auth?: string | null };

/** 主机密钥确认的一步：已信任，或需要用户核对这一级的指纹 */
export type HostKeyStep =
  | { kind: "trusted" }
  | { kind: "confirm"; probeId: string; host: string; level: number; keyType: string; fingerprint: string };

/** 终端后端：Tauri 实现和浏览器预览实现都遵守这个接口 */
export interface TerminalBackend {
  /** 是否是浏览器里的预览实现 */
  readonly preview: boolean;
  open(spec: OpenSpec, cols: number, rows: number, onEvent: (e: TerminalEvent) => void): Promise<string>;
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
}
