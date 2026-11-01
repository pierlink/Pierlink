/**
 * 远程桌面的剪贴板自动同步（和其他远程桌面软件一样）：本机剪贴板一变，就把里面的文字发到远程剪贴板，
 * 远程里右键粘贴、Ctrl+V 都能用，不用先在画面里按 Ctrl+V。本机剪贴板没变时不发，
 * 免得把远程自己复制的文件、图片冲掉。
 *
 * 不发的情况：
 * - 本机剪贴板里是从这个桌面复制来的文字（写进来时打了这个桌面的来源标记）：远程剪贴板里本来就是它，
 *   发回去会让远程的剪贴板只剩文字（Xvnc 接管剪贴板），文件管理器里复制的文件粘出来成了路径。
 *   断开重连、弹到独立窗口、重启 Pierlink、同一个桌面开着两个查看窗口、两次写入完成的先后颠倒都靠它认出来；
 *   Windows 上任何程序再复制都会清掉标记，照样发；别的桌面来的（标记不同）也照样发，跨服务器复制粘贴要用；
 * - 本机剪贴板的这次变化是我们把远程复制的文字写进来造成的（没有标记时靠变化序号和内容认）；
 * - 拿不到变化序号（不是 Windows）时，和上次发过的文字一样，或就是远程来的文字。
 */

/** 读到的本机剪贴板 */
export interface ClipboardRead {
  /** 文字；没有文字时为空字符串 */
  text: string;
  /** 远程桌面写入时打的来源标记；别的程序复制的为 null */
  origin: string | null;
}

/** 同步要用到的读写（测试里换成假的） */
export interface ClipboardIo {
  /** 已经连上远程：没连上时发的会被丢掉，不读也不发 */
  ready(): boolean;
  /** 本机剪贴板的变化序号；拿不到时为 null（只能读出内容比较） */
  sequence(): Promise<number | null>;
  /** 读本机剪贴板的文字和来源标记；被别的程序占着时抛错 */
  read(): Promise<ClipboardRead>;
  /** 这个桌面的来源值（写进本机剪贴板时打的标记）；还不知道时为 null */
  origin(): string | null;
  /** 发到远程剪贴板；没发出去（已断开）返回 false */
  send(text: string): boolean;
}

/**
 * 桌面的来源值：远程复制的文字写进 Windows 剪贴板时打的标记，用来认出“这段文字是从这个桌面来的”。
 * 别的程序也能读剪贴板，所以只放服务器 ID 和桌面键的哈希（SHA-256 前 16 字节），不放明文。
 * @param serverId 服务器 ID
 * @param key 桌面的键（DesktopSession.key）
 * @returns 32 个十六进制字符；算不了（没有 Web Crypto）时为 null，这时不打标记
 */
export async function desktopOrigin(serverId: string, key: string): Promise<string | null> {
  try {
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(`${serverId}\n${key}`));
    return Array.from(new Uint8Array(digest).slice(0, 16), (b) => b.toString(16).padStart(2, "0")).join("");
  } catch {
    return null;
  }
}

export class ClipboardSync {
  /** 最近一次从远程复制过来的文字（之后本机发过别的就作废） */
  private remote: string | null = null;
  /** 远程来的文字写进本机剪贴板后的变化序号：只有这一次变化是我们自己写的，不发回去 */
  private echo: number | null = null;
  /** 最近一次发过去的文字（只在拿不到变化序号时用来判断变没变） */
  private sent: string | null = null;
  /** 上次看过的变化序号 */
  private seen: number | null = null;
  private queue: Promise<void> = Promise.resolve();

  constructor(private readonly io: ClipboardIo) {}

  /** 新的连接：远程剪贴板里有什么不知道，本机剪贴板里的文字再发一次（从这个桌面复制来的除外，见来源标记） */
  reset(): void {
    this.remote = null;
    this.echo = null;
    this.sent = null;
    this.seen = null;
  }

  /**
   * 远程复制了文字（随后写进本机剪贴板）：它不用再发回去；远程剪贴板换了内容，本机之前发过的也要重新发。
   * @param text 远程剪贴板的文字
   */
  fromRemote(text: string): void {
    this.remote = text;
    this.echo = null;
    this.sent = null;
  }

  /**
   * 远程来的文字已经写进本机剪贴板。知道写完时的序号后，只跳过这一次变化：
   * 之后本机又复制了同样的文字（比如远程又复制了文件，想把这段文字贴过去），照样发。
   * @param seq 写完时的变化序号；拿不到时为 null（只能按内容判断）
   */
  written(seq: number | null): void {
    this.echo = seq;
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
    if (seq !== null && seq === this.echo) {
      this.seen = seq;
      return;
    }
    let read: ClipboardRead;
    try {
      read = await this.io.read();
    } catch {
      // 剪贴板被别的程序占着：下次再看
      return;
    }
    if (!this.io.ready()) return;
    const { text, origin } = read;
    // 带着这个桌面的来源标记：是从这个桌面复制来的，不发回去
    const own = origin !== null && origin === this.io.origin();
    // 还不知道写完时的序号（正在写、或拿不到序号），或这次看的序号比写进去的早（读的时候刚好写进来）时，
    // 按内容认出远程来的文字
    const early = this.echo === null || (seq !== null && seq < this.echo);
    if (!text || own || (early && text === this.remote) || (seq === null && text === this.sent)) {
      this.seen = seq;
      return;
    }
    // 没发出去（刚断开）：不记下这个序号，下次再试
    if (!this.io.send(text)) return;
    this.seen = seq;
    this.sent = text;
    // 远程剪贴板现在是这段文字了：之后本机再复制之前从远程来的文字，也要发过去
    this.remote = null;
    this.echo = null;
  }
}
