import { describe, expect, it } from "vitest";
import { ClipboardSync, type ClipboardIo } from "./clipboardSync";

/** 假的本机剪贴板和远程：set 换内容（序号加一），sent 记下发到远程的文字 */
function fake(opts: { seq?: boolean } = {}) {
  const state = { text: "", seq: 1, ready: true, busy: false, reads: 0, sent: [] as string[] };
  const io: ClipboardIo = {
    ready: () => state.ready,
    sequence: async () => (opts.seq === false ? null : state.seq),
    read: async () => {
      state.reads++;
      if (state.busy) throw new Error("剪贴板被占着");
      return state.text;
    },
    send: (text) => {
      if (!state.ready) return false;
      state.sent.push(text);
      return true;
    },
  };
  const set = (text: string) => { state.text = text; state.seq++; };
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

  it("本机又复制了同样的文字也发（和远程桌面连接一样）；重新连接后连远程来的文字也再发一次", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    set("x");
    await sync.check();
    set("x");
    await sync.check();
    expect(state.sent).toEqual(["x", "x"]);
    sync.fromRemote("r");
    set("r");
    await sync.check();
    expect(state.sent).toEqual(["x", "x"]);
    sync.reset();
    await sync.check();
    expect(state.sent).toEqual(["x", "x", "r"]);
  });

  it("只跳过我们写进去的那一次变化：远程又复制了文件之类后，本机再复制同样的文字照样发", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    sync.fromRemote("X");
    set("X");
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
    const sync = new ClipboardSync({ ...io, read: () => new Promise((r) => { release = () => r(state.text); }) });
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
