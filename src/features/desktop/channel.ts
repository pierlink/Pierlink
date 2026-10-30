import type { RfbChannel } from "@novnc/novnc";

/** 到服务器 VNC 的一条字节流（Tauri 或浏览器预览各自实现） */
export interface DesktopTransport {
  /** 发给服务器 */
  send(data: Uint8Array): void;
  /** 断开 */
  close(): void;
}

const OPEN = 1;
const CLOSING = 2;
const CLOSED = 3;

/**
 * 给 noVNC 用的数据通道：形状和 WebSocket 一样（noVNC 只认这 8 个属性），底下接 DesktopTransport。
 * - noVNC 每次 send 给的是它复用的缓冲区，必须立刻拷贝；同一轮里的多次 send 合并成一次发出（每次 IPC 有固定开销）
 * - 握手数据可能在 noVNC 接上 onmessage 之前就到了，先暂存，接上后再交出去
 */
export class DesktopChannel implements RfbChannel {
  binaryType = "arraybuffer";
  protocol = "";
  readyState = OPEN;
  onopen: RfbChannel["onopen"] = null;
  onerror: RfbChannel["onerror"] = null;
  onclose: RfbChannel["onclose"] = null;
  private _onmessage: RfbChannel["onmessage"] = null;
  private inbound: ArrayBuffer[] = [];
  private outbound: Uint8Array[] = [];
  private flushScheduled = false;
  private transport: DesktopTransport | null = null;

  get onmessage(): RfbChannel["onmessage"] {
    return this._onmessage;
  }

  set onmessage(handler: RfbChannel["onmessage"]) {
    this._onmessage = handler;
    if (handler && this.inbound.length) queueMicrotask(() => this.drainInbound());
  }

  /** 接上底层字节流（连接建立后调用） */
  attach(transport: DesktopTransport) {
    this.transport = transport;
  }

  /** 服务器发来的数据 */
  deliver(chunk: ArrayBuffer) {
    if (this.readyState === CLOSED) return;
    this.inbound.push(chunk);
    if (this._onmessage) this.drainInbound();
  }

  /** 服务器那边断开了（reason 为空表示正常结束） */
  remoteClosed(reason = "") {
    if (this.readyState === CLOSED) return;
    this.readyState = CLOSED;
    this.inbound = [];
    this.outbound = [];
    this.onclose?.({ code: reason ? 1006 : 1000, reason, wasClean: !reason });
  }

  send(data: ArrayBuffer | Uint8Array) {
    if (this.readyState !== OPEN) return;
    // noVNC 给的是它复用缓冲区上的视图：先拷贝
    this.outbound.push(data instanceof ArrayBuffer ? new Uint8Array(data.slice(0)) : data.slice());
    if (!this.flushScheduled) {
      this.flushScheduled = true;
      queueMicrotask(() => this.flush());
    }
  }

  close() {
    if (this.readyState === CLOSED || this.readyState === CLOSING) return;
    this.flush();
    this.readyState = CLOSING;
    this.transport?.close();
    // 和 WebSocket 一样，关闭事件异步发出（noVNC 按这个顺序处理状态）
    setTimeout(() => this.remoteClosed(), 0);
  }

  private drainInbound() {
    while (this.inbound.length && this._onmessage && this.readyState !== CLOSED) {
      const chunk = this.inbound.shift()!;
      this._onmessage({ data: chunk });
    }
  }

  private flush() {
    this.flushScheduled = false;
    if (!this.outbound.length || !this.transport) return;
    const parts = this.outbound;
    this.outbound = [];
    if (parts.length === 1) {
      this.transport.send(parts[0]);
      return;
    }
    const total = parts.reduce((n, p) => n + p.length, 0);
    const merged = new Uint8Array(total);
    let at = 0;
    for (const p of parts) {
      merged.set(p, at);
      at += p.length;
    }
    this.transport.send(merged);
  }
}
