// 命令片段的变量和换行处理（纯函数，带单元测试）。
// 变量写法 {{名字}}：名字是字母、数字、_、-（含中文）；同名变量只问一次。
// 不做 shell 转义：用户可能就是要 * 展开等，发送前在对话框里预览最终命令。

const VAR = /\{\{\s*([\p{L}\p{N}_-]+)\s*\}\}/gu;

/**
 * 找出片段里的变量名（按第一次出现的顺序，去重）。
 * @param body 片段内容
 * @returns 变量名列表
 */
export function parseVariables(body: string): string[] {
  const names: string[] = [];
  for (const m of body.matchAll(VAR)) if (!names.includes(m[1])) names.push(m[1]);
  return names;
}

/**
 * 把变量替换成填写的值；没填的变量原样保留。值里的换行同样统一成 \n。
 * @param body 片段内容
 * @param values 变量名 → 值
 * @returns 替换后的命令
 */
export function render(body: string, values: Record<string, string>): string {
  return body.replace(VAR, (whole, name: string) => (name in values ? normalizeNewlines(values[name]) : whole));
}

/**
 * 换行统一成 \n（Windows 复制来的 \r\n、老式 \r 都转换），交给终端粘贴时再转成回车。
 * 不统一的话，\r\n 会被终端变成两次回车，命令执行两遍（异常 F9）。
 * @param text 原文
 * @returns 只含 \n 换行的文本
 */
export function normalizeNewlines(text: string): string {
  return text.replace(/\r\n?/g, "\n");
}

/**
 * 发送前的最终文本：统一换行，去掉末尾多余的空行（“发送并执行”会自己补一个回车）。
 * @param text 替换变量后的命令
 * @returns 要粘贴进终端的文本
 */
export function prepareForSend(text: string): string {
  return normalizeNewlines(text).replace(/\n+$/, "");
}
