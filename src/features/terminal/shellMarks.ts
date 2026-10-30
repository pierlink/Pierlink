import type { HistoryEntry } from "./predict";

/**
 * shell 钩子发来的提示符标记（后端注入到服务器 bash、zsh 和本机 Git Bash 里）：
 * - OSC 133;A 提示符开始、133;B 提示符结束（输入从这里开始）、133;D;<退出码> 上一条命令结束；
 * - 私有 OSC 7701：i;native=0|1（shell 自带预测时为 1）、c;<退出码>;<命令 base64>（上一条命令）。
 * 每个提示符的顺序：D → c → OSC 7（目录）→ A …提示符… B。
 */
export const PIERLINK_OSC = 7701;

export type Mark =
  | { kind: "promptStart" }
  | { kind: "inputStart" }
  | { kind: "commandEnd"; exit: number | null }
  | { kind: "init"; native: boolean }
  | { kind: "command"; exit: number | null; command: string };

function exitCode(text: string | undefined): number | null {
  return text !== undefined && /^\d{1,3}$/.test(text) && Number(text) <= 255 ? Number(text) : null;
}

/**
 * 解析 OSC 133 的内容（不含 “133;”）。其他程序的 shell 集成也会发 133，多出来的参数忽略。
 * @returns 认识的标记；C 和不认识的为 null
 */
export function parse133(data: string): Mark | null {
  const [kind, code] = data.split(";");
  if (kind === "A") return { kind: "promptStart" };
  if (kind === "B") return { kind: "inputStart" };
  if (kind === "D") return { kind: "commandEnd", exit: exitCode(code) };
  return null;
}

/**
 * 解析私有 OSC 7701 的内容（不含 “7701;”）。
 * @returns 认识的标记；格式不对时为 null
 */
export function parseMark(data: string): Mark | null {
  const parts = data.split(";");
  if (parts[0] === "i") return { kind: "init", native: parts.includes("native=1") };
  if (parts[0] !== "c" || parts.length !== 3 || data.length > 4096) return null;
  try {
    const bytes = Uint8Array.from(atob(parts[2]), (c) => c.charCodeAt(0));
    const command = new TextDecoder("utf-8", { fatal: true }).decode(bytes).replace(/\n+$/, "");
    return command ? { kind: "command", exit: exitCode(parts[1]), command } : null;
  } catch {
    return null;
  }
}

/** 预测要用到的终端能力（xterm.js 的子集，测试里用假的） */
export interface TermLike {
  readonly cols: number;
  readonly buffer: {
    readonly active: {
      readonly type: "normal" | "alternate";
      readonly cursorX: number;
      readonly cursorY: number;
      readonly baseY: number;
      getLine(y: number): { readonly isWrapped: boolean; translateToString(trimRight?: boolean, start?: number, end?: number): string } | undefined;
    };
  };
  registerMarker(cursorYOffset?: number): { readonly line: number; readonly isDisposed: boolean; dispose(): void } | undefined;
}

/** 当前输入和画预测的位置 */
export interface InputAt {
  /** 已输入的内容 */
  text: string;
  /** 光标后面连续空着的格数（预测最多画这么宽，不压住 zsh 右侧提示符等） */
  room: number;
}

/**
 * 跟踪一个终端里 shell 的提示符：知道现在是不是在提示符下、输入从哪一格开始。
 * 只认 B 标记之后的输入；提示符被盖掉（Ctrl+R、清屏）、换到全屏程序、光标不在输入末尾时都不给输入。
 */
export class ShellTracker {
  /** 收到过 B：这个会话装上了钩子 */
  integrated = false;
  /** shell 自带预测（zsh-autosuggestions、ble.sh）：只记录不显示 */
  native = false;
  private anchor: { marker: { readonly line: number; readonly isDisposed: boolean; dispose(): void }; col: number; prompt: string } | null = null;

  /**
   * 处理一个标记。
   * @param mark 标记
   * @param term 终端
   * @param cwd 当前目录（收到命令记录时用：它属于上一个提示符所在的目录）
   * @returns 收到命令记录时返回要存的历史，否则 null
   */
  onMark(mark: Mark, term: TermLike, cwd: string | null): HistoryEntry | null {
    switch (mark.kind) {
      case "init":
        this.native = mark.native;
        return null;
      case "promptStart":
      case "commandEnd":
        this.leave();
        return null;
      case "inputStart": {
        this.leave();
        const buf = term.buffer.active;
        const marker = term.registerMarker(0);
        const line = buf.getLine(buf.baseY + buf.cursorY);
        if (!marker || !line) return null;
        this.integrated = true;
        this.anchor = { marker, col: buf.cursorX, prompt: line.translateToString(false, 0, buf.cursorX) };
        return null;
      }
      case "command":
        return { command: mark.command, cwd, exit: mark.exit, at: Date.now() };
    }
  }

  /** 会话重新连接：钩子要等新会话的标记再算装上 */
  restart(): void {
    this.leave();
    this.integrated = false;
    this.native = false;
  }

  /** 用户按了回车（或会话重连）：离开提示符，等下一个 B */
  leave(): void {
    this.anchor?.marker.dispose();
    this.anchor = null;
  }

  /**
   * 当前输入。
   * @param term 终端
   * @returns 在提示符下、光标在输入末尾时返回输入和右边的空位；否则为 null
   */
  input(term: TermLike): InputAt | null {
    const a = this.anchor;
    const buf = term.buffer.active;
    if (!a || a.marker.isDisposed || buf.type !== "normal") return null;
    const head = buf.getLine(a.marker.line);
    // 提示符还在原处：Ctrl+R 的搜索提示、清屏、程序重画都会把它盖掉
    if (!head || head.translateToString(false, 0, a.col) !== a.prompt) return null;
    const cursorLine = buf.baseY + buf.cursorY;
    if (cursorLine < a.marker.line) return null;
    // 光标必须在提示符那一行，或由它自动折行出来的行上
    for (let y = a.marker.line + 1; y <= cursorLine; y++) if (!buf.getLine(y)?.isWrapped) return null;
    if (cursorLine === a.marker.line && buf.cursorX < a.col) return null;
    let text = "";
    for (let y = a.marker.line; y <= cursorLine; y++) {
      const line = buf.getLine(y);
      if (!line) return null;
      text += line.translateToString(false, y === a.marker.line ? a.col : 0, y === cursorLine ? buf.cursorX : term.cols);
    }
    const tail = buf.getLine(cursorLine)!.translateToString(false, buf.cursorX, term.cols);
    const rest = tail.trimStart();
    const room = tail.length - rest.length;
    // 光标后面紧挨着就有字：光标在输入中间，不预测
    if (room === 0) return null;
    // 光标后面隔几格还有字：靠右对齐的（离行尾最多两格）是 zsh 的右侧提示符，其他的是光标后面还有输入
    if (rest && rest.length - rest.trimEnd().length > 2) return null;
    // 再往下一行如果是这一行的折行续行，说明输入还没到头
    if (buf.getLine(cursorLine + 1)?.isWrapped) return null;
    return { text, room };
  }
}
