/**
 * 界面内的密码消息：
 * - requestEntry：转发、资源概览、同步等“等待输入密码”时，请全局密码框为某台服务器弹出；
 * - emitSaved / onSaved：某台服务器有了新密码（终端据此自动重连、各处刷新状态）。
 */
type Listener<T> = (v: T) => void;

function channel<T>() {
  const listeners = new Set<Listener<T>>();
  return {
    emit(v: T) { for (const cb of [...listeners]) cb(v); },
    on(cb: Listener<T>): () => void { listeners.add(cb); return () => { listeners.delete(cb); }; },
  };
}

const entry = channel<string>();
const saved = channel<string>();

export const passwordBus = {
  /** 请全局密码框为这台服务器弹出（用户点了“输入密码”） */
  requestEntry: (serverId: string) => entry.emit(serverId),
  onEntry: entry.on,
  /** 这台服务器有了新密码 */
  emitSaved: (serverId: string) => saved.emit(serverId),
  onSaved: saved.on,
};
