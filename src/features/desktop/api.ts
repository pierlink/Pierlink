import { inTauri } from "../shell/TitleBar";
import type { DesktopTransport } from "./channel";

/** 连到哪里：服务器上的 Unix 套接字，或服务器 127.0.0.1 的端口 */
export type DesktopTarget = { kind: "unix"; path: string } | { kind: "tcp"; port: number };

/** 服务器上一个已有的 VNC 桌面 */
export interface DesktopSession {
  /** 列表里的唯一键（也用来记密码） */
  key: string;
  /** 显示号（:1 → 1） */
  display: number | null;
  /** TigerVNC / TurboVNC / x11vnc / 其他 */
  flavor: "tigervnc" | "turbovnc" | "x11vnc" | "other";
  target: DesktopTarget;
  /** 监听所有地址（不止本机） */
  exposed: boolean;
  /** 由系统服务启动（改参数需要管理员） */
  systemUnit: string | null;
  /** Pierlink 新建的桌面 */
  pierlink: boolean;
  /** Pierlink 桌面的桌面环境名字（如 Xfce Session）；其他为 null */
  name: string | null;
  /** Pierlink 的精简桌面（只有终端；名字按语言显示） */
  light: boolean;
}

/** 服务器上装好的、能新建的桌面环境 */
export interface DesktopEnvironment {
  id: string;
  name: string;
  heavy: boolean;
}

/** 检测结果 */
export interface DesktopDetection {
  sessions: DesktopSession[];
  desktops: DesktopEnvironment[];
  /** 能新建 Pierlink 桌面（有桌面环境、工具齐全；服务端程序新建时再获取） */
  canCreate: boolean;
  /** 新建前要先获取 TigerVNC（服务器上还没有能用的） */
  needsServerProgram: boolean;
  /** 新建桌面必需、服务器上却没有的工具 */
  missingTools: string[];
  /** 能用 TigerVNC 官方包（服务器或这台电脑下载） */
  officialOk: boolean;
  /** 安装完整桌面的命令（root 登录时不带 sudo）；不认识包管理器为 null */
  installCommand: string | null;
  /** 没有桌面环境时能改用精简桌面（只有终端，不用管理员权限） */
  lightAvailable: boolean;
  /** 服务器上正在装软件（apt、dpkg、dnf 等在运行） */
  packageBusy: boolean;
}

/** 安装完整桌面前的检查结果（大小都是字节） */
export interface DesktopInstallCheck {
  /** root；nopass sudo 不用密码；password sudo 要在终端里输密码；none 没有管理员权限 */
  admin: "root" | "nopass" | "password" | "none";
  /** 内存总量、可用内存、CPU 核数；读不到为 0 */
  memory: number;
  availableMemory: number;
  cpus: number;
  /** 要下载、装好后占用多少；estimated 为 true 时是固定的保守估计 */
  download: number;
  installed: number;
  estimated: boolean;
  /** 要写入的每个文件系统：挂载点、剩余、需要（含余量） */
  disks: { mount: string; available: number; need: number }[];
  enoughSpace: boolean;
  /** 内存或 CPU 低于建议值，不建议装 */
  lowSpec: boolean;
  /** 安装命令（和检测结果里的一致） */
  command: string | null;
  /** 能改用精简桌面 */
  lightAvailable: boolean;
}

/** 获取 TigerVNC 的来源，按这个顺序尝试 */
export type AcquireSource = "Installed" | "Apt" | "ServerDownload" | "LocalDownload" | "ManualPackage";

/** 新建桌面的请求 */
export interface DesktopCreateRequest {
  /** 这次新建的编号（取消时用） */
  task: string;
  /** 桌面环境（检测结果 desktops 里的 id） */
  desktopId: string;
  width: number;
  height: number;
  /** 允许这台电脑下载 TigerVNC 官方包再传上去 */
  allowOfficialDownload: boolean;
  /** 用户选的已下载官方包（本机路径） */
  manualPackage: string | null;
}

/** 新建桌面的进度 */
export type DesktopCreateProgress =
  | { kind: "step"; source: AcquireSource }
  | { kind: "bytes"; phase: "download" | "upload"; bytes: number; total: number | null }
  | { kind: "ready" }
  | { kind: "starting" };

/** 获取 TigerVNC 失败时每一步的原因 */
export interface AcquireFailure { source: AcquireSource; error: unknown }

export interface DesktopCreated {
  session: DesktopSession;
  /** 断开后服务器可能结束这个桌面（退出登录时结束用户进程） */
  mayEndOnLogout: boolean;
}

/** 连接前的握手探测 */
export interface DesktopProbe {
  /** 不需要密码 */
  noAuth: boolean;
  /** 查看器支持这种认证 */
  supported: boolean;
}

/** 一条打开的连接 */
export interface DesktopConnection extends DesktopTransport {
  /** 探测结果（连接前做的） */
  probe: DesktopProbe;
}

export interface DesktopApi {
  detect(serverId: string): Promise<DesktopDetection>;
  /** 安装完整桌面前检查服务器（只读）：管理员权限、磁盘、内存和 CPU、要下载和占用多少 */
  installCheck(serverId: string): Promise<DesktopInstallCheck>;
  /** 打开到 target 的连接；onData 收服务器数据，onClosed 在断开时调用（reason 为空表示正常结束） */
  connect(serverId: string, target: DesktopTarget, onData: (chunk: ArrayBuffer) => void, onClosed: (reason: string) => void): Promise<DesktopConnection>;
  /** 记住的密码（没有为 null） */
  savedPassword(serverId: string, key: string): Promise<string | null>;
  savePassword(serverId: string, key: string, password: string): Promise<void>;
  forgetPassword(serverId: string, key: string): Promise<void>;
  /** 新建 Pierlink 桌面；onProgress 报获取和启动的进度 */
  create(serverId: string, request: DesktopCreateRequest, onProgress: (p: DesktopCreateProgress) => void): Promise<DesktopCreated>;
  cancelCreate(task: string): Promise<void>;
  /** 关闭 Pierlink 桌面（结束服务器上的会话） */
  stop(serverId: string, display: number): Promise<void>;
  /** 选择已下载的 TigerVNC 官方包；取消为 null */
  pickPackage(): Promise<string | null>;
}

/** 从 vnc.acquire_failed 错误里取出每一步的失败原因（解析不了就是空数组） */
export function acquireFailures(err: unknown): AcquireFailure[] {
  const params = (err as { code?: string; params?: Record<string, unknown> } | null)?.params;
  if ((err as { code?: string } | null)?.code !== "vnc.acquire_failed" || typeof params?.steps !== "string") return [];
  try {
    const steps = JSON.parse(params.steps) as { Failed?: { source: AcquireSource; error: unknown } }[];
    return steps.flatMap((s) => (s.Failed ? [{ source: s.Failed.source, error: s.Failed.error }] : []));
  } catch {
    return [];
  }
}

let api: Promise<DesktopApi> | null = null;

/** 取接口：Tauri 里用真实后端，浏览器预览用假服务器 */
export function getDesktopApi(): Promise<DesktopApi> {
  api ??= inTauri ? import("./tauriApi").then((m) => m.createTauriDesktopApi()) : Promise.resolve(previewApi());
  return api;
}

/** 预览里的安装命令 */
const PREVIEW_INSTALL = "sudo apt-get update && sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends xfce4 xfce4-terminal xfce4-notifyd elementary-xfce-icon-theme greybird-gtk-theme librsvg2-common xdg-user-dirs dbus-x11 fonts-noto-cjk tigervnc-standalone-server";

/**
 * 浏览器预览：一个已有的 TigerVNC :1（XFCE），连上是假服务器画的桌面。网址参数：
 * ?nodesk 没有已有桌面；?install 没有桌面环境（给“安装桌面”，检查后 8 秒算装好）；?nocreate 什么都装不了；
 * ?noadmin、?nospace、?lowspec、?checkfail 安装前检查的各种结果
 */
function previewApi(): DesktopApi {
  const passwords = new Map<string, string>();
  const query = () => new URLSearchParams(location.search);
  /** 预览里点了“安装”的时间：8 秒后检测到 XFCE */
  let installStarted = 0;
  return {
    async detect() {
      await new Promise((r) => setTimeout(r, 400));
      const installed = installStarted > 0 && Date.now() - installStarted > 8000;
      const bare = (query().has("install") || query().has("nocreate")) && !installed;
      return {
        sessions: query().has("nodesk") ? [] : [{ key: "tcp:5901", display: 1, flavor: "tigervnc", target: { kind: "tcp", port: 5901 }, exposed: false, systemUnit: "tigervnc-dev.service", pierlink: false, name: null, light: false }],
        desktops: bare ? [] : [{ id: "openbox", name: "Openbox", heavy: false }, { id: "xfce", name: "Xfce Session", heavy: false }],
        canCreate: !bare,
        needsServerProgram: !installed,
        missingTools: [],
        officialOk: true,
        installCommand: query().has("nocreate") ? null : PREVIEW_INSTALL,
        lightAvailable: true,
        packageBusy: false,
      };
    },
    async installCheck() {
      await new Promise((r) => setTimeout(r, 1500));
      if (query().has("checkfail")) throw { code: "remote.timeout", params: {} };
      installStarted = Date.now();
      const gib = 1024 ** 3;
      const nospace = query().has("nospace");
      return {
        admin: query().has("noadmin") ? "none" : "password",
        memory: query().has("lowspec") ? 0.95 * gib : 1.88 * gib,
        availableMemory: query().has("lowspec") ? 0.4 * gib : 1.3 * gib,
        cpus: query().has("lowspec") ? 1 : 2,
        download: 129_202_014,
        installed: 379_855 * 1024,
        estimated: false,
        disks: [{ mount: "/", available: nospace ? 0.8 * gib : 25.8 * gib, need: 1.48 * gib }],
        enoughSpace: !nospace,
        lowSpec: query().has("lowspec"),
        command: PREVIEW_INSTALL,
        lightAvailable: true,
      };
    },
    async connect(_serverId, _target, onData, onClosed) {
      const { FakeRfbServer } = await import("./fakeRfb");
      const server = new FakeRfbServer({ password: new URLSearchParams(location.search).has("vncpass") });
      let open = true;
      setTimeout(() => server.start(onData), 0);
      return {
        probe: { noAuth: true, supported: true },
        send: (data) => { if (open) server.receive(data); },
        close: () => { if (!open) return; open = false; server.close(); onClosed(""); },
      };
    },
    async savedPassword(serverId, key) { return passwords.get(`${serverId}/${key}`) ?? null; },
    async savePassword(serverId, key, password) { passwords.set(`${serverId}/${key}`, password); },
    async forgetPassword(serverId, key) { passwords.delete(`${serverId}/${key}`); },
    async create(_serverId, request, onProgress) {
      const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
      onProgress({ kind: "step", source: "Installed" });
      await wait(300);
      onProgress({ kind: "step", source: "Apt" });
      for (let b = 0; b <= 4; b++) { onProgress({ kind: "bytes", phase: "download", bytes: b * 400_000, total: 1_600_000 }); await wait(250); }
      onProgress({ kind: "ready" });
      onProgress({ kind: "starting" });
      await wait(500);
      const fail = new URLSearchParams(location.search).get("createfail");
      // ?createfail=light：精简桌面那一步缺包（不是获取 TigerVNC 失败）
      if (fail === "light") {
        throw { code: "vnc.download_failed", params: { source: "Apt", package: "xterm", reason: "软件源里没有 xterm", reason_key: "vnc.apt_dependency_unavailable" } };
      }
      if (fail !== null) {
        throw { code: "vnc.acquire_failed", params: { steps: JSON.stringify([{ Failed: { source: "Apt", error: { code: "vnc.download_failed", params: { source: "Apt", reason: "软件源连不上" } } } }]) } };
      }
      return { session: { key: "unix:/home/u/.local/state/pierlink/vnc/20.sock", display: 20, flavor: "tigervnc", target: { kind: "unix", path: "/home/u/.local/state/pierlink/vnc/20.sock" }, exposed: false, systemUnit: null, pierlink: true, name: request.desktopId === "xfce" ? "Xfce Session" : "Openbox", light: request.desktopId === "pierlink-light" }, mayEndOnLogout: false };
    },
    async cancelCreate() {},
    async stop() {},
    async pickPackage() { return null; },
  };
}
