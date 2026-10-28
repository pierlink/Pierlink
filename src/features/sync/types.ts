/** 同步功能界面用的视图数据。接入后端后由生成的接口类型转换而来。 */

/** 部署前检查发现的情况（只列需要告诉用户的） */
export type DeployWarning =
  /** 没开 linger：退出登录或服务器重启后服务会停 */
  | { kind: "linger"; user: string }
  /** 没有可用的 systemd 用户服务，改用后台进程方式 */
  | { kind: "noSystemd"; reason: "none" | "userManagerNotRunning" | "busUnreachable" }
  /** 系统设置了断开登录就结束用户进程，且没开 linger */
  | { kind: "killUserProcesses" }
  /** home 在网络文件系统上 */
  | { kind: "networkFs"; fsType: string }
  /** 用户自己也在运行 Syncthing（我们不碰它，只说明） */
  | { kind: "ownSyncthing"; count: number }
  /** 文件监控上限偏低 */
  | { kind: "lowInotify"; limit: number }
  /** 另一台电脑正在部署 */
  | { kind: "lockHeld"; owner: string };

/** 部署无法进行的原因 */
export type DeployBlocker =
  | { kind: "unsupportedArch"; arch: string }
  | { kind: "noexec" }
  | { kind: "instanceNewer"; serverVersion: string; appVersion: string };

export type DeployStepKey = "inspect" | "upload" | "configure" | "start" | "tunnel" | "pair";
export type DeployStepState = "pending" | "running" | "done" | "skipped" | "failed";

export interface DeployStepView {
  key: DeployStepKey;
  /** 这一步的具体内容（路径、版本等），等宽显示 */
  detail: string;
  state: DeployStepState;
  /** 上传进度 0–1 */
  progress?: number;
  /** 失败原因（已翻译） */
  error?: string;
}

export interface DeployReportView {
  serverName: string;
  user: string;
  osName: string | null;
  arch: string;
  /** 将占用的磁盘（MB） */
  diskMb: number;
  /** 服务器上已有的实例：无 / 复用 / 已安装未运行 / 需升级 */
  existing: "none" | "reuse" | "start" | "upgrade";
  /** 升级时：除本电脑外还在使用该实例的电脑 */
  otherComputers: string[];
  warnings: DeployWarning[];
  blocker: DeployBlocker | null;
}

/** 部署对话框的阶段 */
export type DeployPhase = "review" | "running" | "done" | "failed";

/** 同步任务的总体状态 */
export type SyncTaskState = "synced" | "syncing" | "conflict" | "error" | "paused" | "waiting";

/** 任务里的一台服务器 */
export interface SyncEndpointView {
  serverId: string;
  serverName: string;
  remotePath: string;
  mode: "sendReceive" | "receiveOnly";
  /** 这一端的连接状态：在线 / 重连中 / 离线 */
  /** password：等用户输入密码（这台服务器要密码登录，这次运行还没输） */
  link: "online" | "reconnecting" | "offline" | "password";
  /** 重连中时：多少秒后重试 */
  retryInS?: number;
}

export interface SyncConflictView {
  /** 后端原始记录（处理冲突时原样传回，用于核对文件没有在此期间变化） */
  record?: unknown;
  path: string;
  /** 另一版本的文件名（Syncthing 生成的 .sync-conflict-… 文件） */
  otherName: string;
  /** 另一版本来自哪一端 */
  from: string;
}

export interface SyncEventView {
  id: number;
  /** 今天的是 时:分:秒，更早的带上 月-日 */
  time: string;
  /** 改动是谁做的（“本机”或服务器名称），不知道时为 null */
  by: string | null;
  kind: "down" | "up" | "conflict" | "link" | "error" | "deleted";
  /** 多服务器任务里标出是哪台服务器 */
  server: string | null;
  text: string;
}

export interface SyncTaskView {
  id: string;
  name: string;
  localPath: string;
  endpoints: SyncEndpointView[];
  state: SyncTaskState;
  /** 同步中的进度 0–100 */
  percent: number;
  /** 列表里显示的一行说明（已翻译） */
  detail: string;
  /** 因为缺密码而没连上的服务器（显示“输入密码”）；没有时为 null */
  passwordServerId: string | null;
  /** 因为要验证码、后台登不上而没连上的服务器（显示“登录”）；没有时为 null */
  loginServerId: string | null;
  files: number;
  bytes: number;
  /** 已排除的文件数与大小；null 表示暂时未知 */
  excludedFiles: number | null;
  excludedBytes: number | null;
  /** 旧版本保留：null 表示已关闭；-1 表示开启但占用大小未知 */
  versionsBytes: number | null;
  ignoreRules: string[];
  collaboration: boolean;
  watchDelayS: number;
  conflictMode: "ask" | "newest";
  conflicts: SyncConflictView[];
  events: SyncEventView[];
  /** 还有更早的事件可以加载 */
  eventsHasMore: boolean;
  /** 同步失败的文件（路径 + 原因） */
  errors?: { path: string; reason: string }[];
  /** 因一次删除大量文件被自动暂停 */
  guardPaused?: boolean;
  /** 本机一侧的方式（receiveOnly = 本机只接收服务器的变化） */
  localMode: "sendReceive" | "receiveOnly";
  /** 本机只接收时，本机改过、不会上传的项数 */
  localChanges: number;
}

/** 新建任务第一步发现的问题 */
export type PathIssue =
  /** 危险目录（根、home 根、系统目录、.ssh、Pierlink 自己的目录） */
  | { kind: "dangerous"; side: "local" | "remote"; path: string }
  /** 与已有任务的目录重叠 */
  | { kind: "overlap"; side: "local" | "remote"; task: string }
  /** 本地目录在 OneDrive / 网盘 / 网络驱动器里 */
  | { kind: "cloudFolder"; provider: string }
  /** 目录已被别的 Syncthing 管理 */
  | { kind: "otherSyncthing"; side: "local" | "remote" }
  /** 服务器上这个目录已经在被别的电脑同步：改为加入已有同步 */
  | { kind: "joinExisting"; computer: string }
  /** 目录里已有 .stignore（将被用作排除规则的起点） */
  | { kind: "ignoreFile"; side: "local" | "remote" }
  /** 目录是 git 仓库（.stfolder、.stignore 会出现在 git status 里） */
  | { kind: "gitRepository"; side: "local" | "remote" }
  /** 目录不存在或无法读取 */
  | { kind: "unreadable"; side: "local" | "remote" };

/** 开始前预估的结果 */
export interface SyncEstimateView {
  /** 扫描是否完成；未完成时显示进度 */
  done: boolean;
  scannedRemote: number;
  scannedLocal: number;
  upload: { files: number; bytes: number };
  download: { files: number; bytes: number };
  same: number;
  conflicts: { path: string; localTime: string; remoteTime: string; newer: "local" | "remote" }[];
  conflictCount: number;
  remoteTotal: { files: number; bytes: number };
  localTotal: { files: number; bytes: number };
  /** 排除规则生效后的同步量 */
  remoteIncluded: { files: number; bytes: number };
  localIncluded: { files: number; bytes: number };
  included: { path: string; files: number; bytes: number }[];
  excluded: { path: string; reason: "dependency" | "cache" | "sensitive" | "custom" | "git"; bytes: number | null }[];
}

/** 卸载服务器端组件前的清单（由后端按安装清单 dry-run 得出） */
export interface UninstallReportView {
  serverName: string;
  /** 将删除的项（只来自安装清单） */
  remove: { path: string; kind: "program" | "instance" | "service" | "socket"; bytes: number | null }[];
  /** 旧版本与冲突存档的大小：可能含用户内容，默认保留 */
  versionsBytes: number;
  conflictsBytes: number;
  /** 会停止的同步任务（两端文件都保留） */
  tasks: { name: string; remotePath: string; localPath: string }[];
  /** 除本电脑外还在用这个实例同步的电脑 */
  otherComputers: string[];
  /** 用户自己的 Syncthing（不会受影响） */
  ownSyncthing: { binary: string; configDir: string } | null;
  /** 本电脑的专用密钥是否装在这台服务器上 */
  hasDedicatedKey: boolean;
}

export interface UninstallChoice {
  keepVersions: boolean;
  keepConfig: boolean;
  removeKey: boolean;
}
