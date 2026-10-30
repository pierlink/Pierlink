import { inTauri } from "./TitleBar";

/**
 * 弹出系统的“选择文件夹”对话框（浏览器预览里给一个示例路径）。
 * @returns 选中的文件夹绝对路径；用户取消时为 null
 */
export async function pickFolder(): Promise<string | null> {
  if (!inTauri) return "D:\\work\\new-project";
  const { commands } = await import("../../bindings/commands");
  const r = await commands.pickFolder();
  if (r.status === "error") throw r.error;
  return r.data;
}
