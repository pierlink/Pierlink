/** noVNC 1.7（@novnc/novnc）用到的部分：只声明本项目用的属性、方法和事件 */
declare module "@novnc/novnc" {
  /** noVNC 接收的数据通道：形状和 WebSocket 一样（本项目用自己的对象接到 Tauri） */
  export interface RfbChannel {
    binaryType: string;
    protocol: string;
    readyState: number;
    onopen: ((ev?: unknown) => void) | null;
    onmessage: ((ev: { data: ArrayBuffer }) => void) | null;
    onerror: ((ev?: unknown) => void) | null;
    onclose: ((ev: { code: number; reason: string; wasClean?: boolean }) => void) | null;
    send(data: ArrayBuffer | Uint8Array): void;
    close(): void;
  }

  export interface RfbOptions {
    shared?: boolean;
    credentials?: { username?: string; password?: string; target?: string };
  }

  export default class RFB extends EventTarget {
    constructor(target: HTMLElement, urlOrChannel: string | RfbChannel, options?: RfbOptions);
    viewOnly: boolean;
    focusOnClick: boolean;
    clipViewport: boolean;
    dragViewport: boolean;
    scaleViewport: boolean;
    resizeSession: boolean;
    showDotCursor: boolean;
    background: string;
    qualityLevel: number;
    compressionLevel: number;
    readonly capabilities: { power: boolean };
    disconnect(): void;
    sendCredentials(credentials: { username?: string; password?: string; target?: string }): void;
    sendKey(keysym: number, code: string | null, down?: boolean): void;
    focus(options?: FocusOptions): void;
    blur(): void;
    clipboardPasteFrom(text: string): void;
  }
}
