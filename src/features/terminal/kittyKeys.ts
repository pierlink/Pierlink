/**
 * kitty 键盘协议的最小实现（只支持“消除歧义”这一级）：
 * 终端里的程序（Claude Code、Codex 等）开启后，带修饰键的 Enter 发成 `CSI 13;修饰 u`，
 * 它们就能分清 Enter（提交）和 Shift+Enter（换行）。程序没开启时一切照旧（shell 不受影响）。
 * 主屏和备用屏各有一套标志，全屏程序异常退出也不会影响回到主屏后的 shell。
 */

/** 支持的标志：只有 1（消除歧义）；程序要的其他标志不记 */
const SUPPORTED = 1;
/** 每个屏的栈最多存多少层（按协议，满了丢最旧的） */
const MAX_STACK = 16;

interface Screen { flags: number; stack: number[] }

export class KittyKeyboard {
  private screens: Record<"main" | "alt", Screen> = { main: { flags: 0, stack: [] }, alt: { flags: 0, stack: [] } };
  private active: "main" | "alt" = "main";

  private get s(): Screen {
    return this.screens[this.active];
  }

  /** 当前生效的标志 */
  get flags(): number {
    return this.s.flags;
  }

  /** 切换主屏 / 备用屏 */
  setScreen(alt: boolean) {
    this.active = alt ? "alt" : "main";
  }

  /** CSI > flags u：压栈并设置新标志 */
  push(flags: number) {
    const s = this.s;
    if (s.stack.length >= MAX_STACK) s.stack.shift();
    s.stack.push(s.flags);
    s.flags = flags & SUPPORTED;
  }

  /** CSI < n u：弹出 n 层（弹空就回到 0） */
  pop(n: number) {
    const s = this.s;
    for (let i = 0; i < Math.max(1, n); i++) s.flags = s.stack.pop() ?? 0;
  }

  /** CSI = flags ; mode u：1 设置、2 加上、3 去掉 */
  set(flags: number, mode: number) {
    const f = flags & SUPPORTED;
    const s = this.s;
    if (mode === 2) s.flags |= f;
    else if (mode === 3) s.flags &= ~f;
    else s.flags = f;
  }

  /** 全部清零（终端重置时） */
  reset() {
    this.screens = { main: { flags: 0, stack: [] }, alt: { flags: 0, stack: [] } };
  }

  /** CSI ? u 的回答 */
  query(): string {
    return `\x1b[?${this.flags}u`;
  }

  /**
   * 需要按协议单独编码的按键；不需要时返回 null（照常交给 xterm 处理）。
   * 目前只处理带修饰键的 Enter：Shift+Enter → `ESC[13;2u`。
   */
  encode(ev: Pick<KeyboardEvent, "key" | "shiftKey" | "altKey" | "ctrlKey" | "metaKey">): string | null {
    if (!(this.flags & 1) || ev.key !== "Enter" || ev.metaKey) return null;
    const mods = (ev.shiftKey ? 1 : 0) + (ev.altKey ? 2 : 0) + (ev.ctrlKey ? 4 : 0);
    return mods ? `\x1b[13;${mods + 1}u` : null;
  }
}

/** Windows 控制台的“按键”格式（win32-input-mode）：按下、松开。本机 PowerShell、命令提示符按它们自己的原生规则处理 */
const WIN32_SHIFT_ENTER = "\x1b[13;28;13;1;16;1_\x1b[13;28;13;0;16;1_";
const WIN32_CTRL_ENTER = "\x1b[13;28;10;1;8;1_\x1b[13;28;10;0;8;1_";
const WIN32_CTRL_J = "\x1b[74;36;10;1;8;1_\x1b[74;36;10;0;8;1_";

/** 本机终端是哪种 shell（按程序路径判断；本机终端的 id 就是小写的程序完整路径，WSL 后面带 #发行版）。other：Git Bash 等 */
export function localShellFamily(shellId: string): "powershell" | "cmd" | "wsl" | "other" {
  const exe = shellId.split("#")[0].toLowerCase();
  if (/(^|[\\/])(pwsh|powershell)\.exe$/.test(exe)) return "powershell";
  if (/(^|[\\/])cmd\.exe$/.test(exe)) return "cmd";
  if (/(^|[\\/])wsl\.exe$/.test(exe)) return "wsl";
  return "other";
}

/**
 * Shift+Enter、Ctrl+Enter、Ctrl+J 发什么：
 * - 原样交给 Windows 控制台（和 Windows Terminal 一样，不做转换，由程序自己处理）：本机 PowerShell、命令提示符；
 *   本机 Git Bash 里的全屏程序（多半是 Windows 程序，比如在 Git Bash 里开的 Codex）。
 *   1.2.0 以前它们的全屏程序会被换成换行符，控制台又把换行符变成 Ctrl+Enter，本机的 Codex 就不换行了；
 * - 服务器上、WSL 里的程序，和 Git Bash 的提示符：是 Unix 程序，收不到“Shift”这个修饰键，只能换个说法：
 *   程序开了 kitty 协议就按协议发 `ESC[13;2u`（Claude Code、Codex 当换行）；全屏程序（备用屏，含 screen、tmux
 *   里的 Claude Code）发换行符 Ctrl+J（它们的换行键）；shell 里发 Ctrl+V 再接换行符，bash、zsh 插入换行不执行；
 * - 程序自己开了 kitty 协议时（本机控制台里也一样，比如 PowerShell 里开的 ssh 连着服务器上的 Codex）一律按协议发。
 * 返回 null 表示照常处理（其他键，以及不原样交给控制台时的 Ctrl+J：照常发换行符）。
 */
export function newlineKey(
  ev: Pick<KeyboardEvent, "key" | "shiftKey" | "altKey" | "ctrlKey" | "metaKey">,
  where: { kitty: boolean; local: "powershell" | "cmd" | "wsl" | "other" | null; altScreen: boolean },
): string | null {
  if (ev.altKey || ev.metaKey) return null;
  const native = where.local === "powershell" || where.local === "cmd" || (where.local === "other" && where.altScreen);
  const passThrough = native && !where.kitty;
  if (ev.ctrlKey && !ev.shiftKey && ev.key.toLowerCase() === "j") return passThrough ? WIN32_CTRL_J : null;
  if (ev.key !== "Enter" || ev.shiftKey === ev.ctrlKey) return null;
  if (passThrough) return ev.shiftKey ? WIN32_SHIFT_ENTER : WIN32_CTRL_ENTER;
  if (where.kitty) return "\x1b[13;2u";
  if (where.altScreen) return "\n";
  return "\x16\n";
}

/**
 * OSC 52 剪贴板：只接受写入（`c;base64`，目标可以是 c/p/s 等任意组合），读请求（`?`）一律忽略。
 * @returns 要写进剪贴板的文字；不是写入或格式不对时为 null
 */
export function parseOsc52(data: string, maxBytes = 1 << 20): string | null {
  const i = data.indexOf(";");
  if (i < 0) return null;
  const payload = data.slice(i + 1);
  if (!payload || payload === "?" || payload.length > Math.ceil(maxBytes / 3) * 4) return null;
  try {
    const bin = atob(payload);
    const bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0));
    return new TextDecoder("utf-8", { fatal: false }).decode(bytes);
  } catch {
    return null;
  }
}
