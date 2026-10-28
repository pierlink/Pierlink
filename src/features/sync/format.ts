/**
 * 把字节数格式化成易读的大小（1024 进制，保留到最多一位小数）。
 * @param bytes 字节数
 * @returns 如 "46 MB"、"1.3 GB"、"0 B"
 */
/**
 * Windows 盘符统一大写显示（d:\work → D:\work）；其他路径原样返回
 * @param path 本机路径
 * @returns 盘符大写后的路径
 */
export function upperDrive(path: string): string {
  return /^[a-z]:/.test(path) ? path[0].toUpperCase() + path.slice(1) : path;
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "-";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  const text = i === 0 || v >= 100 ? Math.round(v).toString() : (Math.round(v * 10) / 10).toString();
  return `${text} ${units[i]}`;
}

/**
 * 按当前语言加千位分隔符。
 * @param n 数量
 * @param locale 语言代码
 */
export function formatCount(n: number, locale: string): string {
  return new Intl.NumberFormat(locale).format(n);
}
