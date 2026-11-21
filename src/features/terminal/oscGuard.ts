/**
 * 标题这类控制串（ESC ] … 响铃或 ESC \）里夹着的 C1 控制字符（U+0080–U+009F）在写进终端前去掉。
 *
 * 终端组件遇到这些字符会当成“控制串到此为止”，后面的内容就被当成普通文字画到光标所在的位置。
 * 实际碰到的情况：服务器上的 GNU screen 4.x 把程序设的中文标题存坏了（每个字只留最后一个字节），
 * 重新接回会话时把坏掉的标题发出来，里面正好有这样一个字符，屏幕上就多出几个乱码。
 * 控制串以外的内容一个字节都不动。
 */
export interface OscGuard {
  /** 过滤一段输出；没有要去掉的字节时原样返回同一个数组 */
  (data: Uint8Array): Uint8Array;
}

const ESC = 0x1b;
const enum State { Ground, Escape, Osc }

/** 新建一个过滤器：一个终端会话用一个（状态跨输出块保留） */
export function createOscGuard(): OscGuard {
  let state = State.Ground;
  /** 控制串里上一块最后一个字节是 0xC2，还没发出去：要看这一块的第一个字节才知道是不是 C1 控制字符 */
  let heldC2 = false;
  return (data) => {
    // 绝大多数输出不在控制串里、也没有 ESC：不用逐字节看
    if (state === State.Ground && !heldC2 && data.indexOf(ESC) === -1) return data;
    /** 这一块里要去掉的下标 */
    let drops: number[] | null = null;
    /** 上一块留下的 0xC2 要补在这一块最前面 */
    let prepend = false;
    for (let i = 0; i < data.length; i++) {
      const b = data[i];
      if (heldC2) {
        heldC2 = false;
        if (b >= 0x80 && b <= 0x9f) { (drops ??= []).push(i); continue; }
        prepend = true;
      }
      if (state === State.Ground) {
        if (b === ESC) state = State.Escape;
      } else if (state === State.Escape) {
        state = b === 0x5d ? State.Osc : b === ESC ? State.Escape : State.Ground;
      } else if (b === 0x07 || b === 0x18 || b === 0x1a) {
        state = State.Ground;
      } else if (b === ESC) {
        state = State.Escape;
      } else if (b === 0xc2) {
        if (i + 1 === data.length) { heldC2 = true; (drops ??= []).push(i); }
        else if (data[i + 1] >= 0x80 && data[i + 1] <= 0x9f) { (drops ??= []).push(i, i + 1); i++; }
      }
    }
    if (!drops && !prepend) return data;
    const out = new Uint8Array(data.length - (drops?.length ?? 0) + (prepend ? 1 : 0));
    let n = 0;
    if (prepend) out[n++] = 0xc2;
    let next = 0;
    for (let i = 0; i < data.length; i++) {
      if (drops && next < drops.length && drops[next] === i) { next++; continue; }
      out[n++] = data[i];
    }
    return out;
  };
}
