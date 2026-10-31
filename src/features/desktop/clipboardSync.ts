/**
 * 远程桌面的剪贴板自动同步（和其他远程桌面软件一样）：本机剪贴板里有了新的文字，就发到远程剪贴板，
 * 远程里右键粘贴、Ctrl+V 都能用，不用先在画面里按 Ctrl+V。
 *
 * 不发的情况：
 * - 本机剪贴板里是从远程复制过来的文字：发回去会把远程剪贴板里的文件、图片换成文字；
 * - 这次连接已经发过同样的文字：远程之后复制了文件之类（不会通知过来），重复发会把它冲掉。
 */

/** 同步要用到的读写（测试里换成假的） */
export interface ClipboardIo {
  /** 已经连上远程：没连上时发的会被丢掉，不读也不发 */
  ready(): boolean;
  /** 本机剪贴板的变化序号；拿不到时为 null（只能读出内容比较） */
  sequence(): Promise<number | null>;
  /** 读本机剪贴板的文字；没有文字时为空字符串，被别的程序占着时抛错 */
  read(): Promise<string>;
  /** 发到远程剪贴板；没发出去（已断开）返回 false */
  send(text: string): boolean;
}

export class ClipboardSync {
  /** 最近一次从远程复制过来的文字 */
  private remote: string | null = null;
  /** 这次连接已经发过去的文字 */
  private sent: string | null = null;
  /** 上次看过的变化序号 */
  private seen: number | null = null;
  private queue: Promise<void> = Promise.resolve();

  constructor(private readonly io: ClipboardIo) {}

  /** 新的连接：远程剪贴板里有什么不知道，本机剪贴板里的文字再发一次 */
  reset(): void {
    this.sent = null;
    this.seen = null;
  }

  /**
   * 远程复制了文字（随后写进本机剪贴板）：它不用再发回去；远程剪贴板换了内容，本机之前发过的也要重新发。
   * @param text 远程剪贴板的文字
   */
  fromRemote(text: string): void {
    this.remote = text;
    this.sent = null;
  }

  /**
   * 看一次本机剪贴板，有新的文字就发到远程。一次只看一个，正在看时又要看就排在后面。
   * @param poll 是定时看：拿不到变化序号时不读，免得每秒都去读剪贴板
   * @returns 看完（发了或不用发）时完成
   */
  check(poll = false): Promise<void> {
    const run = () => this.run(poll);
    this.queue = this.queue.then(run, run);
    return this.queue;
  }

  private async run(poll: boolean): Promise<void> {
    if (!this.io.ready()) return;
    const seq = await this.io.sequence();
    if (seq === null ? poll : seq === this.seen) return;
    let text: string;
    try {
      text = await this.io.read();
    } catch {
      // 剪贴板被别的程序占着：下次再看
      return;
    }
    if (!this.io.ready()) return;
    this.seen = seq;
    if (!text || text === this.remote || text === this.sent) return;
    if (this.io.send(text)) this.sent = text;
  }
}
