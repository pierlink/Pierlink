import { Channel } from "@tauri-apps/api/core";
import { commands, type AppError, type DesktopChunk, type DesktopCreateProgress, type DesktopSessionDto } from "../../bindings/commands";
import { formatError } from "../../errors";
import type { DesktopApi, DesktopDetection, DesktopSession } from "./api";

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 字节 → base64（发往服务器的只有键盘、鼠标、剪贴板，量很小） */
function toBase64(bytes: Uint8Array): string {
  let text = "";
  for (let i = 0; i < bytes.length; i += 0x8000) text += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(text);
}

const FLAVORS = new Set(["tigervnc", "turbovnc", "x11vnc", "other"]);

function toSession(s: DesktopSessionDto): DesktopSession {
  return {
    key: s.key,
    display: s.display ?? null,
    flavor: (FLAVORS.has(s.flavor) ? s.flavor : "other") as DesktopSession["flavor"],
    target: s.target,
    exposed: s.exposed,
    systemUnit: s.systemUnit ?? null,
    pierlink: s.pierlink,
    name: s.name ?? null,
  };
}


/** Tauri 里的实现：数据经后端 SSH 通道，服务器发来的按原始字节到达 */
export function createTauriDesktopApi(): DesktopApi {
  return {
    async detect(serverId) {
      const d = unwrap(await commands.desktopDetect(serverId));
      const result: DesktopDetection = {
        sessions: d.sessions.map(toSession),
        desktops: d.desktops,
        canCreate: d.canCreate,
        needsServerProgram: d.needsServerProgram,
        missingTools: d.missingTools,
        officialOk: d.officialOk,
        installCommand: d.installCommand ?? null,
        installCommandLight: d.installCommandLight ?? null,
      };
      return result;
    },
    async connect(serverId, target, onData, onClosed) {
      const data = new Channel<ArrayBuffer>();
      data.onmessage = (chunk) => onData(chunk);
      const closed = new Channel<AppError | null>();
      closed.onmessage = (e) => onClosed(e ? formatError(e) : "");
      const r = unwrap(await commands.desktopConnect(serverId, target, data as unknown as Channel<DesktopChunk>, closed));
      const id = r.id;
      // 发送必须按顺序：每次发完再发下一次（并发的命令在后端可能乱序）
      let queue: Promise<unknown> = Promise.resolve();
      let open = true;
      return {
        probe: { noAuth: r.noAuth, supported: r.supported },
        send: (bytes) => {
          if (!open) return;
          const text = toBase64(bytes);
          queue = queue.then(() => commands.desktopSend(id, text)).catch(() => {});
        },
        close: () => {
          if (!open) return;
          open = false;
          // 不排在发送后面：某次发送卡住时（对方不读）关闭也要立刻生效，后端关闭会让卡住的发送返回
          void commands.desktopClose(id);
        },
      };
    },
    async savedPassword(serverId, key) {
      return unwrap(await commands.desktopPassword(serverId, key)) ?? null;
    },
    async savePassword(serverId, key, password) {
      unwrap(await commands.desktopSavePassword(serverId, key, password));
    },
    async forgetPassword(serverId, key) {
      unwrap(await commands.desktopForgetPassword(serverId, key));
    },
    async create(serverId, request, onProgress) {
      const channel = new Channel<DesktopCreateProgress>();
      // f64 在绑定里可能是 null（NaN 序列化成 null），按 0 处理
      channel.onmessage = (p) => onProgress(p.kind === "bytes" ? { ...p, bytes: p.bytes ?? 0, total: p.total ?? null } : p);
      const r = unwrap(await commands.desktopCreate(serverId, request, channel));
      return { session: toSession(r.session), mayEndOnLogout: r.mayEndOnLogout };
    },
    async cancelCreate(task) {
      unwrap(await commands.desktopCreateCancel(task));
    },
    async stop(serverId, display) {
      unwrap(await commands.desktopStop(serverId, display));
    },
    async pickPackage() {
      return unwrap(await commands.desktopPickPackage()) ?? null;
    },
  };
}
