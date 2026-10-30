/**
 * 把文字逐字输入到远程桌面要用的 X 按键编码（keysym）。
 * 拉丁字母等 Latin-1 字符直接用码位；中文等其他字符用 Unicode keysym（0x01000000 + 码位），
 * TigerVNC 的服务端会临时映射成按键，所以不用远程装输入法也能输入中文。
 */

const XK_RETURN = 0xff0d;
const XK_TAB = 0xff09;

/**
 * 一个字符对应的 keysym。
 * @param ch 一个字符（可以是超出 BMP 的字符，如表情）
 * @returns keysym；换行发回车、制表符发 Tab，其他控制字符返回 null（不发）
 */
export function keysymFor(ch: string): number | null {
  const cp = ch.codePointAt(0);
  if (cp === undefined) return null;
  if (ch === "\n") return XK_RETURN;
  if (ch === "\t") return XK_TAB;
  if (cp < 0x20 || (cp >= 0x7f && cp < 0xa0)) return null;
  return cp <= 0xff ? cp : 0x01000000 + cp;
}

/**
 * 要逐字输入的一段文字对应的 keysym 列表（\r\n、\r 都算一个回车）。
 * @param text 文字
 * @param limit 最多多少个字符：太长逐字输入很慢，返回 null 让调用方改用粘贴
 */
export function typingKeysyms(text: string, limit = 2000): number[] | null {
  const out: number[] = [];
  for (const ch of text.replace(/\r\n?/g, "\n")) {
    const k = keysymFor(ch);
    if (k === null) continue;
    out.push(k);
    if (out.length > limit) return null;
  }
  return out;
}
