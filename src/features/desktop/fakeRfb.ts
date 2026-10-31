/**
 * 浏览器预览和测试用的假 VNC 服务器：说 RFB 3.8，画一个仿 XFCE 的桌面（顶栏、时钟、一个浏览器窗口）。
 * 只用 Raw 编码；增量更新每秒刷新一次时钟区域。不在 Tauri 里使用。
 */
export interface FakeRfbOptions {
  width?: number;
  height?: number;
  /** 需要密码（VncAuth）；不校验密码内容，只演示输入流程 */
  password?: boolean;
  name?: string;
}

interface PixelFormat { bpp: number; bigEndian: boolean; rs: number; gs: number; bs: number }

const enc = new TextEncoder();

export class FakeRfbServer {
  private width: number;
  private height: number;
  private readonly password: boolean;
  private readonly name: string;
  private toClient: ((chunk: ArrayBuffer) => void) | null = null;
  private buf = new Uint8Array(0);
  private stage: "version" | "security" | "auth" | "init" | "normal" = "version";
  private format: PixelFormat = { bpp: 32, bigEndian: false, rs: 16, gs: 8, bs: 0 };
  private pixels: Uint8ClampedArray;
  private pending: { x: number; y: number; w: number; h: number } | null = null;
  private timer: ReturnType<typeof setInterval> | null = null;
  /** 客户端发来的剪贴板文字（测试用） */
  clipboard = "";

  constructor(opts: FakeRfbOptions = {}) {
    this.width = opts.width ?? 1280;
    this.height = opts.height ?? 720;
    this.password = !!opts.password;
    this.name = opts.name ?? "preview:1 (XFCE)";
    this.pixels = draw(this.width, this.height);
  }

  /** 开始：发版本号；toClient 收服务器发出的数据 */
  start(toClient: (chunk: ArrayBuffer) => void) {
    this.toClient = toClient;
    this.out(enc.encode("RFB 003.008\n"));
  }

  close() {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    this.toClient = null;
  }

  /** 客户端发来的数据 */
  receive(data: Uint8Array) {
    const merged = new Uint8Array(this.buf.length + data.length);
    merged.set(this.buf);
    merged.set(data, this.buf.length);
    this.buf = merged;
    while (this.step()) { /* 一直处理到数据不够一条消息 */ }
  }

  private take(n: number): Uint8Array | null {
    if (this.buf.length < n) return null;
    const head = this.buf.slice(0, n);
    this.buf = this.buf.slice(n);
    return head;
  }

  private out(bytes: Uint8Array) {
    this.toClient?.(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer);
  }

  /** 处理一条完整消息；数据不够时返回 false */
  private step(): boolean {
    switch (this.stage) {
      case "version": {
        if (!this.take(12)) return false;
        this.out(new Uint8Array(this.password ? [1, 2] : [1, 1]));
        this.stage = "security";
        return true;
      }
      case "security": {
        const t = this.take(1);
        if (!t) return false;
        if (t[0] === 2) {
          this.out(new Uint8Array(16).map((_, i) => (i * 37 + 11) & 0xff));
          this.stage = "auth";
        } else {
          this.out(u32(0));
          this.stage = "init";
        }
        return true;
      }
      case "auth": {
        if (!this.take(16)) return false;
        this.out(u32(0));
        this.stage = "init";
        return true;
      }
      case "init": {
        if (!this.take(1)) return false;
        const name = enc.encode(this.name);
        const msg = new Uint8Array(24 + name.length);
        const v = new DataView(msg.buffer);
        v.setUint16(0, this.width);
        v.setUint16(2, this.height);
        msg.set([32, 24, 0, 1], 4);
        v.setUint16(8, 255); v.setUint16(10, 255); v.setUint16(12, 255);
        msg.set([16, 8, 0], 14);
        v.setUint32(20, name.length);
        msg.set(name, 24);
        this.out(msg);
        this.stage = "normal";
        this.timer = setInterval(() => this.tick(), 1000);
        return true;
      }
      case "normal":
        return this.message();
    }
  }

  private message(): boolean {
    if (!this.buf.length) return false;
    const type = this.buf[0];
    const need = (n: number) => this.buf.length >= n;
    switch (type) {
      case 0: { // SetPixelFormat
        if (!need(20)) return false;
        const m = this.take(20)!;
        this.format = { bpp: m[4], bigEndian: m[6] !== 0, rs: m[14], gs: m[15], bs: m[16] };
        return true;
      }
      case 2: { // SetEncodings
        if (!need(4)) return false;
        const n = new DataView(this.buf.buffer, this.buf.byteOffset).getUint16(2);
        if (!need(4 + 4 * n)) return false;
        this.take(4 + 4 * n);
        return true;
      }
      case 3: { // FramebufferUpdateRequest
        if (!need(10)) return false;
        const m = this.take(10)!;
        const v = new DataView(m.buffer);
        const rect = { x: v.getUint16(2), y: v.getUint16(4), w: v.getUint16(6), h: v.getUint16(8) };
        if (m[1]) this.pending = rect; // 增量：等有变化（时钟）再回
        else this.sendRect(0, 0, this.width, this.height);
        return true;
      }
      case 4: // KeyEvent
        if (!need(8)) return false;
        this.take(8);
        return true;
      case 5: // PointerEvent
        if (!need(6)) return false;
        this.take(6);
        return true;
      case 6: { // ClientCutText
        if (!need(8)) return false;
        const len = new DataView(this.buf.buffer, this.buf.byteOffset).getUint32(4);
        if (len > 1 << 20) { this.buf = new Uint8Array(0); return false; }
        if (!need(8 + len)) return false;
        const m = this.take(8 + len)!;
        this.clipboard = new TextDecoder("latin1").decode(m.slice(8));
        return true;
      }
      default:
        // 不认识的消息：长度未知，丢掉剩下的数据
        this.buf = new Uint8Array(0);
        return false;
    }
  }

  /** 每秒：重画时钟，有增量请求就回时钟所在的区域 */
  private tick() {
    drawClock(this.pixels, this.width);
    if (!this.pending) return;
    this.pending = null;
    this.sendRect(this.width - 120, 0, 120, 28);
  }

  private sendRect(x: number, y: number, w: number, h: number) {
    const bpp = this.format.bpp / 8;
    const msg = new Uint8Array(4 + 12 + w * h * bpp);
    const v = new DataView(msg.buffer);
    v.setUint16(2, 1);
    v.setUint16(4, x); v.setUint16(6, y); v.setUint16(8, w); v.setUint16(10, h);
    v.setInt32(12, 0);
    let at = 16;
    const { rs, gs, bs, bigEndian } = this.format;
    for (let row = y; row < y + h; row++) {
      for (let col = x; col < x + w; col++) {
        const i = (row * this.width + col) * 4;
        const value = (this.pixels[i] << rs) | (this.pixels[i + 1] << gs) | (this.pixels[i + 2] << bs);
        if (bpp === 4) v.setUint32(at, value >>> 0, !bigEndian);
        else if (bpp === 2) v.setUint16(at, value & 0xffff, !bigEndian);
        else msg[at] = value & 0xff;
        at += bpp;
      }
    }
    this.out(msg);
  }
}

function u32(n: number): Uint8Array {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, n);
  return b;
}

/** 画一个仿 XFCE 的桌面；没有 canvas（测试环境）时画纯色块 */
function draw(width: number, height: number): Uint8ClampedArray {
  const canvas = typeof document !== "undefined" ? document.createElement("canvas") : null;
  const ctx = canvas?.getContext("2d") ?? null;
  if (!canvas || !ctx) {
    const px = new Uint8ClampedArray(width * height * 4);
    for (let i = 0; i < px.length; i += 4) px.set([0x40, 0x80, 0xff, 0xff], i);
    return px;
  }
  canvas.width = width;
  canvas.height = height;
  const bg = ctx.createLinearGradient(0, 0, width, height);
  bg.addColorStop(0, "#1d3b5c");
  bg.addColorStop(1, "#3a6ea5");
  ctx.fillStyle = bg;
  ctx.fillRect(0, 0, width, height);
  // 顶栏
  ctx.fillStyle = "#2b2b2b";
  ctx.fillRect(0, 0, width, 28);
  ctx.fillStyle = "#e8e8e8";
  ctx.font = "13px sans-serif";
  ctx.fillText("Applications", 12, 19);
  // 浏览器窗口
  const wx = Math.round(width * 0.08), wy = 60, ww = Math.round(width * 0.84), wh = height - 110;
  ctx.fillStyle = "rgba(0,0,0,0.35)";
  ctx.fillRect(wx + 6, wy + 8, ww, wh);
  ctx.fillStyle = "#dee1e6";
  ctx.fillRect(wx, wy, ww, 36);
  ctx.fillStyle = "#ffffff";
  ctx.fillRect(wx + 10, wy + 6, 220, 30);
  ctx.fillStyle = "#202124";
  ctx.font = "13px sans-serif";
  ctx.fillText("Example Domain", wx + 24, wy + 26);
  ctx.fillStyle = "#ffffff";
  ctx.fillRect(wx, wy + 36, ww, 38);
  ctx.fillStyle = "#f1f3f4";
  ctx.fillRect(wx + 90, wy + 43, ww - 180, 24);
  ctx.fillStyle = "#5f6368";
  ctx.fillText("https://example.com", wx + 104, wy + 60);
  ctx.fillStyle = "#ffffff";
  ctx.fillRect(wx, wy + 74, ww, wh - 74);
  ctx.fillStyle = "#202124";
  ctx.font = "bold 30px sans-serif";
  ctx.fillText("Example Domain", wx + 60, wy + 150);
  ctx.font = "16px sans-serif";
  ctx.fillStyle = "#3c4043";
  ctx.fillText("This domain is for use in documentation examples.", wx + 60, wy + 190);
  ctx.fillStyle = "#1a73e8";
  ctx.fillText("Learn more", wx + 60, wy + 224);
  const px = ctx.getImageData(0, 0, width, height).data;
  drawClock(px, width);
  return px;
}

/** 时钟：顶栏右侧，按当前时间画几个格子（不依赖字体，测试环境也能画） */
function drawClock(px: Uint8ClampedArray, width: number) {
  const s = new Date().getSeconds();
  for (let y = 8; y < 20; y++) {
    for (let x = width - 110; x < width - 10; x++) {
      const i = (y * width + x) * 4;
      const on = Math.floor((x - (width - 110)) / 10) === s % 10;
      px.set(on ? [0x9d, 0xa7, 0xff, 0xff] : [0x2b, 0x2b, 0x2b, 0xff], i);
    }
  }
}
