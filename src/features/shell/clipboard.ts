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
 * @returns 写完时剪贴板的变化序号（Windows）；拿不到时为 null
 */
export async function writeClipboardText(text: string): Promise<number | null> {
  if (!inTauri) { await navigator.clipboard.writeText(text); return null; }
  const r = await commands.clipboardWriteText(text);
  if (r.status === "error") throw r.error;
  return r.data;
}

/** 剪贴板文字和远程桌面写入时打的来源标记 */
export interface MarkedClipboardText {
  /** 文字；没有文字时为空字符串 */
  text: string;
  /** 来源；不是远程桌面写的（Windows 上别的程序又复制过、浏览器预览）为 null */
  origin: string | null;
}

/**
 * 读剪贴板的文字和远程桌面写入时打的来源标记（后端同一次打开剪贴板读出）。浏览器预览里没有来源。
 * @returns 文字和来源；剪贴板被别的程序占着时抛错
 */
export async function readMarkedClipboardText(): Promise<MarkedClipboardText> {
  if (!inTauri) return { text: await navigator.clipboard.readText(), origin: null };
  const r = await commands.clipboardReadDesktop();
  if (r.status === "error") throw r.error;
  return { text: r.data.text ?? "", origin: r.data.origin ?? null };
}

/**
 * 写入远程桌面复制来的文字，同时打上来源标记（哪个桌面来的）；Windows 上别的程序再复制时标记就没了。
 * 浏览器预览里只写文字。
 * @param text 文字
 * @param origin 来源值（桌面的哈希，见 desktopOrigin）
 * @returns 写完时剪贴板的变化序号（Windows）；拿不到时为 null
 */
export async function writeMarkedClipboardText(text: string, origin: string): Promise<number | null> {
  if (!inTauri) { await navigator.clipboard.writeText(text); return null; }
  const r = await commands.clipboardWriteDesktop(text, origin);
  if (r.status === "error") throw r.error;
  return r.data;
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
