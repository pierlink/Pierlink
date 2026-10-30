import type { IDecoration, IDisposable, IMarker, Terminal } from "@xterm/xterm";
import { historyOf, onHistoryLoaded, remember } from "./history";
import { nextWord, suggest } from "./predict";
import { parse133, parseMark, PIERLINK_OSC, ShellTracker, type Mark } from "./shellMarks";

/** 东亚宽字符（占两格）；只用来排预测文字，不影响终端本身 */
const WIDE = /[ᄀ-ᅟ⺀-〾ぁ-㏿㐀-䶿一-鿿ꀀ-꓏가-힣豈-﫿︰-﹏＀-｠￠-￦]|[\u{1f300}-\u{1f64f}\u{1f900}-\u{1f9ff}\u{20000}-\u{3fffd}]/u;

/** 字符占几格 */
function cells(ch: string): number {
  return WIDE.test(ch) ? 2 : 1;
}

/**
 * 截到最多 room 格。放不下时最后一格换成“…”：按 → 会把整条（包括看不见的部分）填进输入行，先让人知道后面还有
 * @param text 要显示的预测
 * @param room 光标后面空着的格数
 * @returns 显示的文字和占的格数
 */
export function fit(text: string, room: number): { text: string; width: number } {
  let width = 0;
  for (const ch of text) width += cells(ch);
  if (width <= room) return { text, width };
  if (room < 1) return { text: "", width: 0 };
  width = 0;
  let out = "";
  for (const ch of text) {
    const w = cells(ch);
    if (width + w > room - 1) break;
    out += ch;
    width += w;
  }
  return { text: `${out}…`, width: width + 1 };
}

export interface PredictionOptions {
  /** 历史范围；为 null 时不预测也不记录 */
  scope: string | null;
  /** 现在能不能显示预测（设置开着、没在广播、没交给 AI） */
  enabled: () => boolean;
  /** 现在要不要记下执行过的命令（这个窗格开着预测：交给 AI、这台服务器关了预测时不记） */
  record: () => boolean;
  /** 当前目录（不知道时为 null） */
  cwd: () => string | null;
}

/**
 * 一个终端的命令预测：听 shell 钩子的标记，在光标后面用浅色画出预测（xterm 装饰层，不写进终端内容），
 * 只在预测显示时接管 →（补全整条）和 Ctrl+→（补一个词）；其他时候按键原样交给 shell。
 */
export class Prediction {
  private readonly tracker = new ShellTracker();
  private readonly disposables: IDisposable[] = [];
  private deco: IDecoration | null = null;
  private marker: IMarker | null = null;
  /** 正在显示的预测里还没输入的部分 */
  private rest: string | null = null;
  private frame = 0;

  constructor(private readonly term: Terminal, private readonly opts: PredictionOptions) {
    if (!opts.scope) return;
    const parser = term.parser;
    this.disposables.push(
      parser.registerOscHandler(133, (data) => { this.onMark(parse133(data)); return false; }),
      parser.registerOscHandler(PIERLINK_OSC, (data) => { this.onMark(parseMark(data)); return true; }),
      term.onWriteParsed(() => this.schedule()),
      term.onCursorMove(() => this.schedule()),
      term.buffer.onBufferChange(() => this.schedule()),
      { dispose: onHistoryLoaded(() => this.schedule()) },
    );
  }

  /** 这个会话装上了 shell 钩子（收到过提示符标记） */
  get integrated(): boolean {
    return this.tracker.integrated;
  }

  private onMark(mark: Mark | null): void {
    if (!mark || !this.opts.scope) return;
    const entry = this.tracker.onMark(mark, this.term, this.opts.cwd());
    // 只记装了钩子的会话里的命令：没收到过提示符标记就来的记录，多半是程序输出里伪造的
    if (entry && this.tracker.integrated && this.opts.record()) remember(this.opts.scope, entry);
    this.schedule();
  }

  /** 用户输入了一段（键盘、粘贴、命令片段）：有回车就离开提示符，等下一个 B 标记 */
  onUserData(data: string): void {
    if (/[\r\n]/.test(data)) {
      this.tracker.leave();
      this.hide();
    }
  }

  /** 会话结束或重新连接：之前的提示符和钩子都不算了 */
  reset(): void {
    this.tracker.restart();
    this.hide();
  }

  /**
   * 键盘事件：预测显示时接管 → 和 Ctrl+→。
   * @returns false 表示已经处理（xterm 不再处理）；null 表示不管，照常处理
   */
  handleKey(ev: KeyboardEvent): false | null {
    if (this.rest === null || ev.isComposing || ev.key !== "ArrowRight" || ev.altKey || ev.metaKey || ev.shiftKey) return null;
    ev.preventDefault();
    if (ev.type === "keydown") {
      const text = ev.ctrlKey ? nextWord(this.rest) : this.rest;
      this.hide();
      this.term.input(text, true);
    }
    return false;
  }

  /** 合并到下一帧重新算（输出、光标移动、历史读完都会触发） */
  schedule(): void {
    if (this.frame) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.refresh();
    });
  }

  private refresh(): void {
    const scope = this.opts.scope;
    if (!scope || this.tracker.native || !this.opts.enabled()) return this.hide();
    const at = this.tracker.input(this.term);
    const full = at && suggest(historyOf(scope), at.text, this.opts.cwd());
    if (!at || !full) return this.hide();
    const shown = fit(full.slice(at.text.length), at.room);
    if (!shown.text) return this.hide();
    if (this.rest === full.slice(at.text.length) && this.deco && !this.deco.isDisposed && this.marker?.line === this.term.buffer.active.baseY + this.term.buffer.active.cursorY) return;
    this.draw(shown.text, shown.width, full.slice(at.text.length));
  }

  private draw(text: string, width: number, rest: string): void {
    this.hide();
    const buf = this.term.buffer.active;
    const marker = this.term.registerMarker(0);
    if (!marker) return;
    const deco = this.term.registerDecoration({ marker, x: buf.cursorX, width, layer: "top" });
    if (!deco) { marker.dispose(); return; }
    this.marker = marker;
    this.deco = deco;
    this.rest = rest;
    const { fontFamily, fontSize, theme } = this.term.options;
    deco.onRender((el) => {
      if (el.dataset.ghost === text) return;
      el.dataset.ghost = text;
      el.classList.add("term-ghost");
      el.setAttribute("aria-hidden", "true");
      el.style.fontFamily = fontFamily ?? "";
      el.style.color = theme?.brightBlack ?? "";
      el.style.fontSize = `${fontSize ?? 13}px`;
      el.style.lineHeight = el.style.height;
      // 每个字按格宽排，保证和终端的格子对齐（宽字符占两格）
      const cell = parseFloat(el.style.width) / width;
      el.replaceChildren(...[...text].map((ch) => {
        const span = document.createElement("span");
        span.textContent = ch;
        if (Number.isFinite(cell) && cell > 0) span.style.width = `${cell * cells(ch)}px`;
        return span;
      }));
    });
  }

  /** 收起预测 */
  hide(): void {
    this.rest = null;
    this.deco?.dispose();
    this.marker?.dispose();
    this.deco = null;
    this.marker = null;
  }

  dispose(): void {
    if (this.frame) cancelAnimationFrame(this.frame);
    this.hide();
    this.tracker.leave();
    this.disposables.forEach((d) => d.dispose());
  }
}
