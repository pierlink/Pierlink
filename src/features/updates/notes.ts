/**
 * 发布说明是中英两段：先中文，一行 `---`，再英文。按界面语言只取对应的一段；
 * 没有分隔线（旧版本的说明）时原样返回。
 * @param notes 更新清单里的说明全文
 * @param language 界面语言（如 zh-CN、en）
 * @returns 要显示的说明
 */
export function pickNotes(notes: string, language: string): string {
  const parts = notes.split(/\r?\n-{3,}\r?\n/);
  if (parts.length < 2) return notes.trim();
  const english = language.toLowerCase().startsWith("en");
  return (english ? parts.slice(1).join("\n---\n") : parts[0]).trim();
}
