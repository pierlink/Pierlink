import { describe, expect, it } from "vitest";
import { ClipboardSync, desktopOrigin, type ClipboardIo } from "./clipboardSync";

/** 这个桌面的来源值 */
const ME = "this-desktop";

/**
 * 假的本机剪贴板和远程：set 换内容（序号加一），sent 记下发到远程的文字。
 * set 不带来源是 Windows 上的程序复制的；带来源是远程桌面写进来的（ME 是这个桌面）
 */
function fake(opts: { seq?: boolean } = {}) {
  const state = { text: "", origin: null as string | null, seq: 1, ready: true, busy: false, reads: 0, sent: [] as string[] };
  const io: ClipboardIo = {
    ready: () => state.ready,
    sequence: async () => (opts.seq === false ? null : state.seq),
    read: async () => {
      state.reads++;
      if (state.busy) throw new Error("剪贴板被占着");
      return { text: state.text, origin: state.origin };
    },
    origin: () => ME,
    send: (text) => {
      if (!state.ready) return false;
      state.sent.push(text);
      return true;
    },
  };
  const set = (text: string, origin: string | null = null) => { state.text = text; state.origin = origin; state.seq++; };
  return { state, io, set };
}

describe("远程桌面剪贴板自动同步", () => {
  it("连上后发本机剪贴板；没变就不读也不发；变了再发", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    set("a");
    await sync.check();
    expect(state.sent).toEqual(["a"]);
    await sync.check(true);
    await sync.check();
    expect(state.reads).toBe(1);
    set("b");
    await sync.check(true);
    expect(state.sent).toEqual(["a", "b"]);
  });

  it("从远程复制过来的文字不发回去；远程换了内容后，本机之前发过的文字再复制时重新发", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    set("local");
    await sync.check();
    sync.fromRemote("remote");
    set("remote");
    await sync.check();
    expect(state.sent).toEqual(["local"]);
    set("local");
    await sync.check();
    expect(state.sent).toEqual(["local", "local"]);
  });

  it("远程复制过的文字，本机发过别的之后再复制它，照样发过去", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    sync.fromRemote("X");
    set("X");
    await sync.check();
    set("Y");
    await sync.check();
    set("X");
    await sync.check();
    expect(state.sent).toEqual(["Y", "X"]);
  });

  it("本机又复制了同样的文字也发（和远程桌面连接一样）；重新连接后，Windows 上复制的文字再发一次", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    set("x");
    await sync.check();
    set("x");
    await sync.check();
    expect(state.sent).toEqual(["x", "x"]);
    sync.reset();
    await sync.check();
    expect(state.sent).toEqual(["x", "x", "x"]);
  });

  it("重新连接（弹到独立窗口、重启 Pierlink 也一样）后，剪贴板里还是这个桌面来的文字（带标记）：不发回去；Windows 上再复制就发", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    sync.fromRemote("r");
    set("r", ME);
    sync.written(state.seq);
    await sync.check();
    // 新的连接不记得 r 是远程来的，只能靠标记认出
    sync.reset();
    await sync.check();
    await sync.check(true);
    expect(state.sent).toEqual([]);
    set("r");
    await sync.check();
    expect(state.sent).toEqual(["r"]);
  });

  it("同一个桌面开着两个查看窗口、各把远程的文字写进来一次：两边都不发回去", async () => {
    const { state, io, set } = fake();
    const main = new ClipboardSync(io);
    const popped = new ClipboardSync(io);
    main.fromRemote("R");
    popped.fromRemote("R");
    set("R", ME);
    main.written(state.seq);
    set("R", ME);
    popped.written(state.seq);
    await main.check();
    await popped.check();
    expect(state.sent).toEqual([]);
  });

  it("远程很快复制了两次、两次写入完成的先后颠倒（记成了较早的序号）：不发回去", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    sync.fromRemote("A");
    sync.fromRemote("B");
    set("A", ME);
    const first = state.seq;
    set("B", ME);
    const second = state.seq;
    sync.written(second);
    sync.written(first);
    await sync.check();
    expect(state.sent).toEqual([]);
  });

  it("别的桌面复制来的文字（来源不同）照样发：跨服务器复制粘贴", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    set("from-other", "other-desktop");
    await sync.check();
    expect(state.sent).toEqual(["from-other"]);
  });

  it("只跳过我们写进去的那一次变化：远程又复制了文件之类后，本机再复制同样的文字照样发", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    sync.fromRemote("X");
    set("X", ME);
    sync.written(state.seq);
    await sync.check();
    expect(state.sent).toEqual([]);
    set("X");
    await sync.check();
    expect(state.sent).toEqual(["X"]);
  });

  it("读剪贴板的时候远程的文字刚好写进来：不把它发回去", async () => {
    const { state, io, set } = fake();
    let release: () => void = () => undefined;
    const sync = new ClipboardSync({ ...io, read: () => new Promise((r) => { release = () => r({ text: state.text, origin: state.origin }); }) });
    set("Y");
    const pending = sync.check();
    await new Promise((r) => setTimeout(r, 0));
    sync.fromRemote("X");
    set("X");
    sync.written(state.seq);
    release();
    await pending;
    await sync.check();
    expect(state.sent).toEqual([]);
  });

  it("没发出去（刚断开）时下次再试", async () => {
    const { state, io, set } = fake();
    let accept = false;
    const sync = new ClipboardSync({ ...io, send: (text) => accept && io.send(text) });
    set("a");
    await sync.check();
    expect(state.sent).toEqual([]);
    accept = true;
    await sync.check(true);
    expect(state.sent).toEqual(["a"]);
  });

  it("没连上时不读不发；连上前读到的不算发过", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    set("a");
    state.ready = false;
    await sync.check();
    expect(state.reads).toBe(0);
    state.ready = true;
    await sync.check();
    expect(state.sent).toEqual(["a"]);
  });

  it("剪贴板被占着读不到时，下次再读", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    set("a");
    state.busy = true;
    await sync.check();
    state.busy = false;
    await sync.check(true);
    expect(state.sent).toEqual(["a"]);
  });

  it("拿不到变化序号时：定时看不读，其他时候读出来比较", async () => {
    const { state, io, set } = fake({ seq: false });
    const sync = new ClipboardSync(io);
    set("a");
    await sync.check(true);
    expect(state.reads).toBe(0);
    await sync.check();
    await sync.check();
    expect(state.reads).toBe(2);
    expect(state.sent).toEqual(["a"]);
  });

  it("桌面的来源值是哈希：同一个桌面相同，换服务器或桌面就不同，不含明文", async () => {
    const key = "unix:/home/u/.local/state/pierlink/vnc/20.sock";
    const a = await desktopOrigin("srv-1", key);
    expect(a).toMatch(/^[0-9a-f]{32}$/);
    expect(await desktopOrigin("srv-1", key)).toBe(a);
    expect(await desktopOrigin("srv-2", key)).not.toBe(a);
    expect(await desktopOrigin("srv-1", "tcp:5901")).not.toBe(a);
  });

  it("空剪贴板不发；同时要看几次时一个接一个", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    await sync.check();
    expect(state.sent).toEqual([]);
    set("a");
    await Promise.all([sync.check(), sync.check(), sync.check(true)]);
    expect(state.sent).toEqual(["a"]);
    expect(state.reads).toBe(2);
  });
});
