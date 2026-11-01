/** 读终端格子用到的最小接口：xterm 的缓冲区满足它，测试里用假的 */
export interface CellLike {
  isInverse(): number;
  getWidth(): number;
}
export interface LineLike {
  getCell(x: number, cell?: CellLike): CellLike | undefined;
}
export interface BufferLike {
  viewportY: number;
  getLine(y: number): LineLike | undefined;
  getNullCell?(): CellLike;
}

/**
 * 找程序自己画的假光标：可见区域里唯一一个单独的反色格（宽字符算一格）。
 * Claude Code 等程序藏起真实光标、在输入处画一个反色格；输入法的组字框和候选框要放在这里。
 * 连成一段的反色（选中项、状态条）不算；找到不止一个时不知道是哪个，返回 null。
 * @param buffer 当前屏幕的缓冲区
 * @param cols 列数
 * @param rows 行数
 * @returns 假光标所在的列和它在可见区域里的行；没有或不唯一时为 null
 */
export function findFakeCursor(buffer: BufferLike, cols: number, rows: number): { x: number; y: number } | null {
  const probe = buffer.getNullCell?.();
  const inverse = (line: LineLike, x: number) => x >= 0 && x < cols && !!line.getCell(x, probe)?.isInverse();
  let found: { x: number; y: number } | null = null;
  for (let y = 0; y < rows; y++) {
    const line = buffer.getLine(buffer.viewportY + y);
    if (!line) continue;
    for (let x = 0; x < cols; x++) {
      if (!inverse(line, x)) continue;
      const width = Math.max(1, line.getCell(x, probe)?.getWidth() ?? 1);
      let end = x + width;
      while (inverse(line, end)) end++;
      if (end === x + width && !inverse(line, x - 1)) {
        if (found) return null;
        found = { x, y };
      }
      x = end - 1;
    }
  }
  return found;
}

/**
 * 组字框和隐藏输入框该放的位置（CSS 像素，相对终端屏幕）。
 * 先对准程序自己画的光标（有的话），再保证组字框的右边不伸出终端：快到右边界时整体往左挪到贴着右边。
 * 一次算出最终位置、只写这一个值：观察者因为这次写入重新进来时算出的还是同一个值，不会来回改。
 * @param anchor 程序自己画的光标那一格的位置；没有时为 null（照 xterm 摆的位置）
 * @param current xterm 现在摆的左边位置
 * @param width 组字框的宽度；组字框没在用时为 null（不用夹）
 * @param screen 终端屏幕的宽度
 * @returns 左边、上边位置；不用改的那一项为 null
 */
export function placeIme(
  anchor: { left: number; top: number } | null,
  current: number,
  width: number | null,
  screen: number,
): { left: number | null; top: number | null } {
  let left = anchor ? anchor.left : current;
  if (width !== null) left = Math.min(left, Math.max(0, screen - width));
  return { left: anchor || left !== current ? left : null, top: anchor ? anchor.top : null };
}
