/**
 * 手机屏幕的颜色：电脑端给的每一行只带 SGR（ESC [ … m）颜色和样式码，这里拆成一段段带样式的文字。
 * 颜色用 CSS 变量（--ansi-0 … --ansi-15，深浅色各一套）或直接的 rgb()。
 */

export interface Style {
  fg?: string;
  bg?: string;
  bold?: boolean;
  dim?: boolean;
  italic?: boolean;
  underline?: boolean;
  inverse?: boolean;
}

export interface Segment {
  text: string;
  style: Style;
}

/** 0–15 号颜色用主题里的变量 */
function basic(n: number): string {
  return `var(--ansi-${n})`;
}

/** 256 色：0–15 基本色，16–231 是 6×6×6 的色块，232–255 是灰阶 */
export function color256(n: number): string {
  if (n < 16) return basic(n);
  if (n < 232) {
    const i = n - 16;
    const level = (v: number) => (v === 0 ? 0 : 55 + v * 40);
    return `rgb(${level(Math.floor(i / 36))}, ${level(Math.floor(i / 6) % 6)}, ${level(i % 6)})`;
  }
  const g = 8 + (n - 232) * 10;
  return `rgb(${g}, ${g}, ${g})`;
}

/** 解析一串 SGR 参数，改写 style；不认识的参数忽略 */
function applySgr(params: number[], style: Style): Style {
  const s = { ...style };
  for (let i = 0; i < params.length; i++) {
    const p = params[i];
    if (p === 0) { for (const k of Object.keys(s) as (keyof Style)[]) delete s[k]; }
    else if (p === 1) s.bold = true;
    else if (p === 2) s.dim = true;
    else if (p === 3) s.italic = true;
    else if (p === 4) s.underline = true;
    else if (p === 7) s.inverse = true;
    else if (p === 22) { delete s.bold; delete s.dim; }
    else if (p === 23) delete s.italic;
    else if (p === 24) delete s.underline;
    else if (p === 27) delete s.inverse;
    else if (p >= 30 && p <= 37) s.fg = basic(p - 30);
    else if (p === 39) delete s.fg;
    else if (p >= 40 && p <= 47) s.bg = basic(p - 40);
    else if (p === 49) delete s.bg;
    else if (p >= 90 && p <= 97) s.fg = basic(p - 90 + 8);
    else if (p >= 100 && p <= 107) s.bg = basic(p - 100 + 8);
    else if (p === 38 || p === 48) {
      const key = p === 38 ? "fg" : "bg";
      if (params[i + 1] === 5 && i + 2 < params.length) {
        s[key] = color256(params[i + 2] & 255);
        i += 2;
      } else if (params[i + 1] === 2 && i + 4 < params.length) {
        const [r, g, b] = params.slice(i + 2, i + 5).map((v) => Math.min(255, Math.max(0, v)));
        s[key] = `rgb(${r}, ${g}, ${b})`;
        i += 4;
      }
    }
  }
  return s;
}

/**
 * 把一行拆成带样式的段。只认 ESC [ 数字;… m，其他控制字符去掉。
 * @param line 电脑端给的一行
 * @returns 依次排列的段（相邻同样式的会合并）
 */
export function parseSgrLine(line: string): Segment[] {
  const out: Segment[] = [];
  let style: Style = {};
  let text = "";
  const flush = () => {
    if (!text) return;
    const last = out[out.length - 1];
    if (last && JSON.stringify(last.style) === JSON.stringify(style)) last.text += text;
    else out.push({ text, style });
    text = "";
  };
  const re = /\x1b\[([0-9;:]*)m/g;
  let at = 0;
  for (let m = re.exec(line); m; m = re.exec(line)) {
    text += line.slice(at, m.index).replace(/[\x00-\x08\x0b-\x1f\x7f]/g, "");
    at = m.index + m[0].length;
    flush();
    const params = m[1] === "" ? [0] : m[1].split(/[;:]/).map((x) => (x === "" ? 0 : Number(x)));
    style = applySgr(params, style);
  }
  text += line.slice(at).replace(/[\x00-\x08\x0b-\x1f\x7f]/g, "");
  flush();
  return out;
}

/** 去掉颜色码后的纯文字（复制、识别选项用） */
export function stripSgr(line: string): string {
  return line.replace(/\x1b\[[0-9;:]*m/g, "");
}
