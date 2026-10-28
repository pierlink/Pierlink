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
