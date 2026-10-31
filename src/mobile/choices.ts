/**
 * 认出屏幕最下面等着回答的选项，手机上直接给成大按钮：
 * - Claude Code、Codex 等的编号菜单（“❯ 1. Yes”“  2. No, and tell Claude …”）：按数字键选；
 * - shell 和安装脚本常见的 (y/n)、[Y/n]：按 y 或 n。
 */

export interface Choice {
  /** 要发的按键（数字或 y、n） */
  key: string;
  /** 按钮上的字 */
  label: string;
  /** 当前高亮的那一项（编号菜单里带 ❯ 或 > 的） */
  current?: boolean;
  /** 按完还要回车（apt、pip、read -p 的 [Y/n] 要回车才生效；编号菜单按数字就生效） */
  enter?: boolean;
}

/** 编号选项：可选的高亮标记，然后“数字. 文字” */
const NUMBERED = /^\s*[│┃|]?\s*([❯›>▶➤*])?\s*(\d)[.)]\s+(.+?)\s*[│┃|]?\s*$/;
/** 问是或否 */
const YES_NO = /[([](y\/n|Y\/n|y\/N|yes\/no)[)\]]\s*[:?]?\s*$/i;

/**
 * 从屏幕最后几行里找选项。
 * 编号的几行要么带高亮标记（Claude Code、Codex 的菜单总有 ❯ 或 ›），要么就在屏幕最底下（bash 的 select 等）；
 * Claude 回答里的编号列表后面还有输入框和状态栏，不算选项。
 * @param lines 屏幕上的行（纯文字，去掉颜色码），按从上到下
 * @param scan 最多往上看几行非空行
 * @returns 选项；没有时为 null
 */
export function detectChoices(lines: string[], scan = 14): Choice[] | null {
  const tail: string[] = [];
  for (let i = lines.length - 1; i >= 0 && tail.length < scan; i--) {
    if (lines[i].trim()) tail.unshift(lines[i]);
  }
  if (!tail.length) return null;
  // 编号菜单：连续的 1、2、3…（中间夹一两行说明或折行也可以）
  let best: Choice[] | null = null;
  let run: Choice[] = [];
  /** run 最后一项在 tail 里的位置 */
  let runEnd = -1;
  let gap = 0;
  const finish = () => {
    if (run.length >= 2 && run.length <= 9 && (run.some((c) => c.current) || runEnd >= tail.length - 2)) best = run;
    run = [];
    gap = 0;
  };
  tail.forEach((line, i) => {
    const m = NUMBERED.exec(line);
    const item: Choice | null = m && { key: m[2], label: m[3].replace(/\s{2,}/g, " "), ...(m[1] ? { current: true } : {}) };
    if (item && Number(item.key) === run.length + 1) {
      run.push(item);
      runEnd = i;
      gap = 0;
    } else if (item && item.key === "1") {
      finish();
      run = [item];
      runEnd = i;
    } else if (run.length && ++gap > 2) finish();
  });
  finish();
  if (best) return best;
  const last = tail[tail.length - 1];
  if (YES_NO.test(last)) return [{ key: "y", label: "y", enter: true }, { key: "n", label: "n", enter: true }];
  return null;
}
