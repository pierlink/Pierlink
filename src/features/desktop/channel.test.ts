import { describe, expect, it, vi } from "vitest";
import { DesktopChannel } from "./channel";
import { FakeRfbServer } from "./fakeRfb";

const tick = () => new Promise((r) => setTimeout(r, 0));

describe("桌面数据通道", () => {
  it("noVNC 接上之前到的数据先暂存，接上后按顺序交出", async () => {
    const ch = new DesktopChannel();
    ch.deliver(new Uint8Array([1, 2]).buffer);
    ch.deliver(new Uint8Array([3]).buffer);
    const got: number[] = [];
    ch.onmessage = (e) => got.push(...new Uint8Array(e.data));
    await tick();
    ch.deliver(new Uint8Array([4]).buffer);
    expect(got).toEqual([1, 2, 3, 4]);
  });

  it("同一轮的多次发送拷贝后合并成一次", async () => {
    const send = vi.fn();
    const ch = new DesktopChannel();
    ch.attach({ send, close: () => {} });
    const reused = new Uint8Array([9, 9]);
    ch.send(reused);
    reused.set([7, 7]); // noVNC 会复用缓冲区：发出去的必须是当时的内容
    ch.send(reused);
    await tick();
    expect(send).toHaveBeenCalledTimes(1);
    expect([...send.mock.calls[0][0]]).toEqual([9, 9, 7, 7]);
  });

  it("关闭后异步通知一次，之后不再收发", async () => {
    const close = vi.fn();
    const send = vi.fn();
    const ch = new DesktopChannel();
    ch.attach({ send, close });
    const onclose = vi.fn();
    ch.onclose = onclose;
    ch.close();
    expect(onclose).not.toHaveBeenCalled();
    await tick();
    expect(onclose).toHaveBeenCalledTimes(1);
    expect(close).toHaveBeenCalledTimes(1);
    ch.send(new Uint8Array([1]));
    ch.remoteClosed("x");
    await tick();
    expect(send).not.toHaveBeenCalled();
    expect(onclose).toHaveBeenCalledTimes(1);
  });

  it("服务器断开带原因时是非正常关闭", () => {
    const ch = new DesktopChannel();
    const onclose = vi.fn();
    ch.onclose = onclose;
    ch.remoteClosed("连接断开了");
    expect(onclose).toHaveBeenCalledWith({ code: 1006, reason: "连接断开了", wasClean: false });
  });
});

describe("预览用的假 VNC 服务器", () => {
  it("走完 RFB 3.8 握手并回一帧画面", () => {
    const server = new FakeRfbServer({ width: 4, height: 2 });
    const out: Uint8Array[] = [];
    server.start((c) => out.push(new Uint8Array(c)));
    expect(new TextDecoder().decode(out[0])).toBe("RFB 003.008\n");
    server.receive(new TextEncoder().encode("RFB 003.008\n"));
    expect([...out[1]]).toEqual([1, 1]); // 只有一种认证：None
    server.receive(new Uint8Array([1]));
    expect([...out[2]]).toEqual([0, 0, 0, 0]); // 认证成功
    server.receive(new Uint8Array([1])); // ClientInit：共享
    const init = new DataView(out[3].buffer);
    expect([init.getUint16(0), init.getUint16(2)]).toEqual([4, 2]);
    // 客户端要求 R 在低位（noVNC 的像素格式），再要整帧
    const fmt = new Uint8Array(20);
    fmt.set([32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16], 4);
    server.receive(fmt);
    server.receive(new Uint8Array([3, 0, 0, 0, 0, 0, 0, 4, 0, 2]));
    const fbu = out[4];
    expect(fbu.length).toBe(16 + 4 * 2 * 4);
    // 测试环境没有 canvas：画的是 #4080ff
    expect([...fbu.slice(16, 20)]).toEqual([0x40, 0x80, 0xff, 0]);
    server.close();
  });

  it("要密码时先发挑战，收到回答后认证成功", () => {
    const server = new FakeRfbServer({ password: true });
    const out: Uint8Array[] = [];
    server.start((c) => out.push(new Uint8Array(c)));
    server.receive(new TextEncoder().encode("RFB 003.008\n"));
    expect([...out[1]]).toEqual([1, 2]);
    server.receive(new Uint8Array([2]));
    expect(out[2].length).toBe(16);
    server.receive(new Uint8Array(16));
    expect([...out[3]]).toEqual([0, 0, 0, 0]);
    server.close();
  });
});
