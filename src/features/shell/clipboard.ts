import { commands } from "../../bindings/commands";
import { inTauri } from "./TitleBar";

/**
 * 读取剪贴板文字。在应用里由后端直接读取系统剪贴板（网页方式读取会让 WebView2 弹出权限确认）；
 * 浏览器预览里用网页接口。
 * @returns 文字；剪贴板里没有文字时为空字符串
 */
export async function readClipboardText(): Promise<string> {
  if (!inTauri) return navigator.clipboard.readText();
  const r = await commands.clipboardReadText();
  if (r.status === "error") throw r.error;
  return r.data ?? "";
}

/**
 * 剪贴板的变化序号：内容每变一次就变（Windows 由后端取，不用打开剪贴板）。用来判断要不要重新读。
 * @returns 序号；拿不到时（其他系统、浏览器预览）为 null，这时只能读出内容来比较
 */
export async function clipboardSequence(): Promise<number | null> {
  if (!inTauri) return null;
  try {
    return await commands.clipboardSequence();
  } catch {
    return null;
  }
}

/**
 * 把文字写进剪贴板。在应用里由后端直接写系统剪贴板：网页方式要求窗口在前台，程序发来的复制请求
 * （OSC 52、远程桌面同步）可能在窗口不在前台时到达，那时会失败。浏览器预览里用网页接口。
 * @param text 要写入的文字
 */
export async function writeClipboardText(text: string): Promise<void> {
  if (!inTauri) return navigator.clipboard.writeText(text);
  const r = await commands.clipboardWriteText(text);
  if (r.status === "error") throw r.error;
}

/**
 * 用系统默认浏览器打开网址（只允许 http、https，由后端检查）。浏览器预览里开新标签页。
 * @param url 网址
 */
export async function openInBrowser(url: string): Promise<void> {
  if (!inTauri) { window.open(url, "_blank", "noopener"); return; }
  const r = await commands.openInBrowser(url);
  if (r.status === "error") throw r.error;
}
