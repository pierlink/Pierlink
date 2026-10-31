// 广播输入的判断逻辑（纯函数，便于单元测试）：危险命令、密码提示、按键行缓冲。
// 只能看到本应用收到的按键：用上箭头翻历史、!!、Ctrl+R、Tab 补全出来的命令看不到，所以危险命令判定是“尽力而为”。

/** 危险命令：广播时按回车前要再确认一次（和后端 AI 接入的判断同一份清单、同一组测试向量） */
// sh/bash/zsh 的 -c：可以和其他开关合写（-lc、-ec），前面可以有 -o pipefail、-euo pipefail、--login 这类开关
const SHELL_C = String.raw`(?:sh|bash|zsh)(?:\s+(?:-[a-zA-Z]*o\s+[\w-]+|-[a-zA-Z]+|--[\w-]+))*?\s+-[a-zA-Z]*c[a-zA-Z]*`;
// cmd 的 /c 或 /k：前面可以有 /d、/q 这类开关
const CMD_C = String.raw`cmd(?:\.exe)?(?:\s+/\w)*?\s+/[ck]`;
// powershell/pwsh 的 -Command（可缩写到 -c）：前面可以有 -NoProfile、-ExecutionPolicy Bypass 这类开关
const PWSH_C = String.raw`(?:powershell|pwsh)(?:\.exe)?(?:\s+-\w+(?:\s+[^-\s"']\S*)?)*?\s+-c\w*`;
// 命令名必须在命令开头（行首，或 ; & | ( 换行之后），避免“grep reboot log”这类误判；
// 开头可以套 sudo（带选项和选项的值）、上面三种包装、env、nohup 等
const START = String.raw`(?:^|[;&|(\r\n])\s*(?:(?:sudo(?:\s+-\S+(?:\s+[^-\s]\S*)?)*|${CMD_C}|${SHELL_C}|${PWSH_C}|nohup|exec|time|env(?:\s+\w+=\S*)*)\s+)*`;
// 整套不区分大小写（Windows 命令和参数本来就不区分；Unix 命令大写也照样算）；git branch 的 -D 另外判断（见 BRANCH）
const DANGEROUS: RegExp[] = [
  // rm 带 -r / -R / -f（含组合写法 -rf、-fr、--recursive、--force）
  String.raw`rm\s+(?:-[a-zA-Z]*[rRf][a-zA-Z]*|--recursive|--force)\b`,
  String.raw`(?:reboot|shutdown|poweroff|halt)\b`,
  String.raw`init\s+[06]\b`,
  String.raw`mkfs(?:\.\w+)?\b`,
  String.raw`dd\b.*\bof=/dev/`,
  String.raw`(?:chmod|chown)\s+(?:-[a-zA-Z]*R[a-zA-Z]*|--recursive)\b.*\s/(?:\s|$)`,
  String.raw`git\s+push\b.*(?:\s-f\b|--force)`,
  String.raw`git\s+reset\b[^;&|\r\n]*\s--hard(?:\s|$)`,
  String.raw`git\s+clean\b`,
  String.raw`git\s+checkout\b[^;&|\r\n]*\s(?:--\s+\S|\.(?:\s|$))`,
  String.raw`git\s+stash\s+(?:drop|clear)\b`,
  String.raw`(?:curl|wget)\b[^;&|\r\n]*\|\s*(?:sh|bash|zsh)\b`,
  String.raw`(?:Invoke-Expression|iex|Set-ExecutionPolicy)\b`,
  String.raw`reg\s+add\s+(?:HKLM|HKEY_LOCAL_MACHINE)\b`,
  String.raw`(?:Set-ItemProperty|Remove-Item)\b[^;&|\r\n]*\s(?:HKLM|HKEY_LOCAL_MACHINE):`,
  String.raw`schtasks(?:\.exe)?\b[^;&|\r\n]*\s/create\b`,
  String.raw`Register-ScheduledTask\b`,
  String.raw`Stop-Process\b[^;&|\r\n]*\s-Force(?:\s|$)`,
  String.raw`taskkill(?:\.exe)?\b[^;&|\r\n]*\s/F(?:\s|$)`,
  String.raw`docker\s+system\s+prune\b`,
  String.raw`docker\s+rm\b[^;&|\r\n]*\s(?:-[a-z]*f[a-z]*|--force)(?:\s|$)`,
  String.raw`docker\s+volume\s+rm\b`,
  String.raw`(?:Remove-Item|ri)\b[^;&|\r\n]*\s-f(?:orce|orc|or|o)?(?:\s|$)`,
  String.raw`crontab\s+-[a-z]*r[a-z]*(?:\s|$)`,
  String.raw`systemctl\s+(?:disable|mask)\b`,
  String.raw`truncate\b[^;&|\r\n]*\s(?:-s\s*0|--size(?:=|\s+)0)(?:\s|$)`,
  String.raw`kubectl\s+delete\b`,
  String.raw`(?:drop\s+(?:table|database|schema)\b|truncate\s+table\b)`,
  // PowerShell 的删除和它的别名，-Recurse 可以缩写到 -r
  String.raw`(?:Remove-Item|ri|rm|del|erase|rd|rmdir)\b.*\s-r(?:ecurse|ecurs|ecur|ecu|ec|e)?\b`,
  String.raw`(?:rmdir|rd|del|erase)\b.*\s/s\b`,
  String.raw`(?:format\s+[a-z]:|diskpart\b|Stop-Computer\b|Restart-Computer\b|Clear-Disk\b|Format-Volume\b|reg\s+delete\b|bcdedit\b|vssadmin\s+delete\b)`,
].map((pattern) => new RegExp(START + pattern, "i")).concat([
  />\s*\/dev\/(?:sd|nvme|vd|hd)/,
  /:\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:/,
]);
// 下面两条要看参数，单独判断：git branch 的 -D 区分大小写（-d 只删已合并的分支）；
// git restore 不带 --staged 才会丢掉工作区的修改。不用 (?-i:) 写法，老一些的浏览器内核不认
const BRANCH = new RegExp(START + String.raw`git\s+branch\b([^;&|\r\n]*)`, "gi");
const RESTORE = new RegExp(START + String.raw`git\s+restore\b([^;&|\r\n]*)`, "gi");

/** 看参数才能判断的几条：git branch -D、不带 --staged 的 git restore */
function dangerousArgs(masked: string): boolean {
  for (const m of masked.matchAll(BRANCH)) {
    // 参数后面紧跟的字符（命令分隔符或换行）也带上，和后端的“-D 后面是空白或结尾”一致
    const next = masked.charAt((m.index ?? 0) + m[0].length);
    if (/\s-D(?:\s|$)/.test(m[1] + next)) return true;
  }
  for (const m of masked.matchAll(RESTORE)) {
    if (!m[1].split(/\s+/).some((arg) => arg.toLowerCase() === "--staged")) return true;
  }
  return false;
}

/** 引号前面是 sh -c、cmd /c、powershell -Command 这类包装：引号里的才是真正执行的命令 */
const WRAPPER_END = new RegExp(String.raw`(?:^|[\s;&|(])(?:${SHELL_C}|${CMD_C}|${PWSH_C})\s*$`, "i");

/** 一行命令按引号扫描的结果 */
interface Scan {
  /** 引号里的文字换成占位字符：引号里的 ; | 不是命令边界（例如 echo "x; reboot"），占位符也让两侧单词不会拼到一起 */
  masked: string;
  /** 顶层引号里的内容和它前面的文字 */
  segments: { before: string; inner: string }[];
  /** 到行尾引号还没闭合 */
  unterminated: boolean;
}

/** 按引号扫描 line。escapes：反斜杠是否转义下一个字符（sh 的规则；PowerShell 和 cmd 里反斜杠不转义） */
function scan(line: string, escapes: boolean): Scan {
  let masked = "";
  const segments: { before: string; inner: string }[] = [];
  let quote: { q: string; start: number } | null = null;
  let inner = "";
  let quoteStart = 0;
  let escaped = false;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (escaped) {
      masked += "x";
      if (quote) inner += c;
      escaped = false;
      continue;
    }
    if (!quote) {
      if (escapes && c === "\\") { escaped = true; masked += "x"; }
      else if (c === "'" || c === '"') { quote = { q: c, start: i }; inner = ""; quoteStart = masked.length; masked += "x"; }
      else masked += c;
    } else {
      masked += "x";
      if (escapes && c === "\\" && quote.q === '"') escaped = true;
      else if (c === quote.q) {
        // 只影响参数判断的几个字面量留下来（"."、"--staged"、注册表 HKLM 路径），引号里的命令边界和命令文字仍然屏蔽
        const token = quotedToken(inner);
        if (token) masked = masked.slice(0, quoteStart) + token;
        segments.push({ before: line.slice(0, quote.start), inner });
        quote = null;
      }
      else inner += c;
    }
  }
  return { masked, segments, unterminated: quote !== null };
}

/** 引号里是这些字面量时保留成什么（和后端一样只把 ASCII 字母转小写比较） */
function quotedToken(inner: string): string | null {
  const lower = inner.replace(/[A-Z]/g, (c) => c.toLowerCase());
  if (lower === "." || lower === "--staged") return lower;
  if (lower.startsWith("hklm:") || lower.startsWith("hklm\\") || lower.startsWith("hkey_local_machine\\")) return "HKLM:";
  return null;
}

function dangerousScan(s: Scan, depth: number): boolean {
  if (!s.masked) return false;
  if (DANGEROUS.some((r) => r.test(s.masked)) || dangerousArgs(s.masked)) return true;
  if (depth >= 3) return false;
  return s.segments.some(({ before, inner }) => WRAPPER_END.test(before) && dangerousAt(inner, depth + 1));
}

function dangerousAt(line: string, depth: number): boolean {
  const trimmed = line.trim();
  const shell = scan(trimmed, true);
  // Windows 路径里的反斜杠不是转义符（没加引号的注册表路径、以 \ 结尾的带引号路径 "D:\work\tmp\"）：按反斜杠不转义再看一遍
  return dangerousScan(shell, depth) || ((shell.unterminated || trimmed.includes("\\")) && dangerousScan(scan(trimmed, false), depth));
}

/**
 * 判断一行命令是否危险（广播时回车前要确认）。套在 sh -c、cmd /c、powershell -Command 引号里的命令也会取出来判断。
 * @param line 用户输入的一行命令
 * @returns 是否命中危险命令清单
 */
export function isDangerous(line: string): boolean {
  return dangerousAt(line, 0);
}

/**
 * 根据终端最近的输出判断是否正在等用户输入密码（这时不能把广播的按键送进去）。
 * 只看最后一行非空文字；sudo 自定义提示、非英文提示可能认不出来，误判也只会让这个窗格退出广播。
 * @param tail 终端最近一段输出（可含控制字符）
 * @param typed 用户在这一行已经输入、还没按回车的文字：最后一行以它结尾时只是输入的回显（例如正在敲 echo password:），不算提示
 * @returns 最后一行像不像密码提示
 */
export function looksLikePasswordPrompt(tail: string, typed = ""): boolean {
  // 去掉颜色等控制序列，再取最后一行
  // eslint-disable-next-line no-control-regex
  const plain = tail.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "").replace(/\x1b\][^\x07]*(\x07|\x1b\\)/g, "");
  const lines = plain.split(/\r?\n|\r/).map((l) => l.trimEnd()).filter(Boolean);
  const last = lines[lines.length - 1] ?? "";
  if (!last) return false;
  if (typed.trim() && last.endsWith(typed.trimEnd())) return false;
  // 打印出来的“Password:”（echo 命令）不是提示
  if (/(?:^|[$#>]\s+)echo\s/.test(last)) return false;
  if (/^\[sudo\] password for /i.test(last)) return true;
  if (/(?:password|passphrase|passcode|verification code|authenticator code|one-time code|(?:2fa|mfa) code|密码|口令|验证码|令牌)[^\n]{0,40}[:：]\s*$/i.test(last)) return true;
  // pin、otp、token 这三个短词容易出现在普通输出里（“Fetching token:”），要像在提问才算
  if (/(?:\b(?:enter|input|type|paste|your|2fa|mfa)\b|请输入)[^\n]{0,30}\b(?:pin|otp|token)\b[^\n]{0,20}[:：]\s*$/i.test(last)) return true;
  if (/^(?:pin|otp|token)\s*[:：]\s*$/i.test(last)) return true;
  return /^Enter passphrase/i.test(last);
}

/**
 * 按键行缓冲：跟着用户的按键维护“自上次回车以来输入的这一行”，回车时交出这一行。
 * 处理退格、Ctrl+U（清行）、Ctrl+W（删一个词）、Ctrl+C（放弃这一行）；方向键等控制序列忽略。
 */
export class LineBuffer {
  private line = "";

  /** 当前这一行（还没按回车的部分） */
  get current(): string {
    return this.line;
  }

  /** 清空（例如广播开始或结束时） */
  reset(): void {
    this.line = "";
  }

  /**
   * 输入一段按键数据。
   * @param data 终端交给程序的数据（可能是一个键，也可能是一次粘贴）
   * @returns 这段数据里每次回车时完整的那一行（按顺序）；数据里没有回车时为空数组
   */
  feed(data: string): string[] {
    const done: string[] = [];
    let i = 0;
    while (i < data.length) {
      const c = data[i];
      if (c === "\x1b") {
        // 括号粘贴的起止标记：里面的内容照常当作输入
        if (data.startsWith("\x1b[200~", i) || data.startsWith("\x1b[201~", i)) { i += 6; continue; }
        // 其他控制序列（方向键、功能键等）：跳过整个序列
        const m = /^\x1b(\[[0-9;?]*[ -/]*[@-~]|O.|.)/.exec(data.slice(i));
        i += m ? m[0].length : 1;
        continue;
      }
      if (c === "\r" || c === "\n") {
        done.push(this.line);
        this.line = "";
        // \r\n 算一次回车
        if (c === "\r" && data[i + 1] === "\n") i += 1;
      } else if (c === "\x7f" || c === "\b") {
        this.line = Array.from(this.line).slice(0, -1).join("");
      } else if (c === "\x15" || c === "\x03") {
        this.line = "";
      } else if (c === "\x17") {
        this.line = this.line.replace(/\S+\s*$/, "");
      } else if (c >= " ") {
        this.line += c;
      }
      i += 1;
    }
    return done;
  }
}
