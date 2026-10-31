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

  it("同样的文字这次连接发过就不再发（远程复制了文件之类时不冲掉）；重新连接后再发一次", async () => {
    const { state, io, set } = fake();
    const sync = new ClipboardSync(io);
    set("x");
    await sync.check();
    set("x");
    await sync.check();
    expect(state.sent).toEqual(["x"]);
    sync.reset();
    await sync.check();
    expect(state.sent).toEqual(["x", "x"]);
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
