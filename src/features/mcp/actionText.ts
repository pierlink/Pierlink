/**
 * AI 确认框里“要做什么”的文字：后端用中文写固定的标题和标注（写入文件：路径、旧文字、新文字……），
 * 英文界面把这些换成英文。命令、路径和要写的内容原样显示，不做任何改动。
 * 电脑上的确认框和手机上的确认页共用。
 */

type Rule = [RegExp, string | ((match: string, ...groups: string[]) => string)];

const yes = (v: string) => (v === "true" ? "yes" : "no");

/** 只替换第一处：标题在开头，标注各出现一次（后面要写的内容里碰巧有同样的文字也不动）；多处修改的编号标注每处一个，全部替换 */
const RULES: Rule[] = [
  [/^写入文件：/, "Write file: "],
  [/^写入启动文件夹会在登录时运行程序：/, "Writing to a Startup folder runs programs at sign-in: "],
  [/^写入脚本、程序或快捷方式可能执行命令：/, "Writing scripts, programs or shortcuts can run commands: "],
  [/^写入 SSH、Git 或 shell 的配置可能让它们执行命令：/, "Writing SSH, Git or shell configuration can make them run commands: "],
  [/^创建目录（含父目录）：/, "Create folder (and missing parents): "],
  [/^删除路径：/, "Delete: "],
  [/^重命名：/, "Rename: "],
  [/^复制：/, "Copy: "],
  [/^用默认程序打开：/, "Open with the default app: "],
  [/^用默认浏览器打开网址：/, "Open in the default browser: "],
  [/^把图片传到服务器的临时目录，并把路径粘贴进终端：/, "Upload an image to the server's temp folder and paste its path into the terminal: "],
  [/(^|\n)传输文件：/, (_m, start) => `${start}Transfer file: `],
  [/\n大小：(\d+) 字节；覆盖已有目标：(true|false)$/, (_m, bytes, v) => `\nSize: ${bytes} bytes; overwrite an existing target: ${yes(v)}`],
  [/\n文件 (\d+) 个、目录 (\d+) 个，共 (\d+) 字节；覆盖已有目标：(true|false)/, (_m, files, dirs, bytes, v) => `\n${files} files and ${dirs} folders, ${bytes} bytes; overwrite existing targets: ${yes(v)}`],
  [/\n会写入要确认的文件（共 (\d+) 项）：/, (_m, n) => `\nWrites files that need confirmation (${n} in total):`],
  [/^（终端里可能有交给 AI 之前或你刚打的、还没回车的文字，AI 按回车，实际执行的内容未知）/, "(The terminal may hold text typed before it was handed to AI, or text you just typed without pressing Enter. AI pressed Enter, so what runs is unknown.)"],
  [/^（翻历史后回车，内容未知）$/, "(Enter after recalling history: the command is unknown)"],
  [/^（AI 撤销了修改或粘回了删掉的文字，按回车实际执行的内容未知）/, "(AI undid an edit or pasted back deleted text, so what runs on Enter is unknown)"],
  [/\n共 (\d+) 处修改/, (_m, n) => `\n${n} edits`],
  [/\n第 (\d+) 处旧文字：\n/g, (_m, n) => `\nEdit ${n}, old text:\n`],
  [/\n第 (\d+) 处新文字：\n/g, (_m, n) => `\nEdit ${n}, new text:\n`],
  [/\n…（还有 (\d+) 处）$/, (_m, n) => `\n… (${n} more)`],
  [/\n旧文字：\n/, "\nOld text:\n"],
  [/\n新文字：\n/, "\nNew text:\n"],
  [/\n内容：(\d+) 字节，前 6 行：\n/, (_m, bytes) => `\nContent: ${bytes} bytes, first 6 lines:\n`],
  [/\n内容：(\d+) 字节（content_base64，原样写入）$/, (_m, bytes) => `\nContent: ${bytes} bytes (content_base64, written as-is)`],
  [/\n递归删除：(true|false)$/, (_m, v) => `\nRecursive: ${yes(v)}`],
  [/\n覆盖已有目标：(true|false)（目录连同内容移动，不递归删除）$/, (_m, v) => `\nOverwrite an existing target: ${yes(v)} (a folder moves with its contents)`],
  [/\n（在目录 ([^\n]*) 里执行）$/, (_m, cwd) => `\n(runs in ${cwd})`],
];

/** 截断的预览后面的“…（共 N 字）” */
const TRUNCATED = /…（共 (\d+) 字）/g;

/**
 * 按界面语言显示确认框里的操作说明。
 * @param action 后端给的操作说明
 * @param language 界面语言（i18n.language）；中文原样返回
 * @returns 要显示的文字
 */
export function localizeAction(action: string, language: string): string {
  if (language.startsWith("zh")) return action;
  let text = action;
  for (const [pattern, replacement] of RULES) {
    text = typeof replacement === "string" ? text.replace(pattern, replacement) : text.replace(pattern, replacement);
  }
  return text.replace(TRUNCATED, (_m, n: string) => `… (${n} characters in total)`);
}
