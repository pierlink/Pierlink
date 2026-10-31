/**
 * 本机终端里用到的 Windows 路径转换：拖文件进终端时填什么，“在资源管理器中打开”打开哪个文件夹。
 * 本机终端的 ID 是程序路径（小写）；WSL 是“wsl.exe 路径#发行版名”。
 */

/** 本机终端的种类：决定路径怎么写、怎么加引号 */
export type LocalShellStyle =
  | { kind: "powershell" }
  | { kind: "cmd" }
  | { kind: "wsl"; distro: string }
  /** Git Bash 这类 MSYS 环境：C:\a 写成 /c/a */
  | { kind: "msys" }
  | { kind: "other" };

/**
 * 按终端 ID 判断种类。
 * @param shellId 本机终端 ID
 */
export function shellStyle(shellId: string): LocalShellStyle {
  const hash = shellId.indexOf("#");
  const exe = (hash < 0 ? shellId : shellId.slice(0, hash)).toLowerCase();
  if (/(^|[\\/])(pwsh|powershell)\.exe$/.test(exe)) return { kind: "powershell" };
  if (/(^|[\\/])cmd\.exe$/.test(exe)) return { kind: "cmd" };
  if (/(^|[\\/])wsl\.exe$/.test(exe) && hash >= 0) return { kind: "wsl", distro: shellId.slice(hash + 1) };
  if (/(^|[\\/])bash\.exe$/.test(exe)) return { kind: "msys" };
  return { kind: "other" };
}

/** C:\a\b → { drive: "c", rest: "a/b" }；不是盘符路径时返回 null */
function driveParts(path: string): { drive: string; rest: string } | null {
  const m = /^([A-Za-z]):[\\/]?(.*)$/.exec(path);
  return m ? { drive: m[1].toLowerCase(), rest: m[2].replace(/\\/g, "/") } : null;
}

/** \\wsl.localhost\发行版\x 或 \\wsl$\发行版\x → /x（发行版不同时返回 null） */
function wslSharePath(path: string, distro: string): string | null {
  const m = /^\\\\(?:wsl\.localhost|wsl\$)\\([^\\]+)(.*)$/i.exec(path);
  if (!m || m[1].toLowerCase() !== distro.toLowerCase()) return null;
  return m[2].replace(/\\/g, "/") || "/";
}

/** bash 里的单引号写法：需要时整体加单引号，里面的 ' 写成 '\'' */
function bashQuote(s: string): string {
  return /^[A-Za-z0-9_\-.,/:@%+=]+$/.test(s) ? s : `'${s.replace(/'/g, "'\\''")}'`;
}

/** PowerShell：需要时加单引号，里面的 ' 写成 '' */
function powershellQuote(s: string): string {
  return /^[A-Za-z0-9_\-.:\\/]+$/.test(s) ? s : `'${s.replace(/'/g, "''")}'`;
}

/** 命令提示符：有空格或特殊字符时加双引号（Windows 文件名里不会有双引号） */
function cmdQuote(s: string): string {
  return /[\s&()[\]{}^=;!'+,`~]/.test(s) ? `"${s}"` : s;
}

/**
 * 拖进本机终端的一个 Windows 路径，按终端种类改写并加引号。
 * @param style 终端种类
 * @param path Windows 路径（如 C:\Users\me\a b.txt）
 * @returns 可以直接放进命令行的写法
 */
export function shellPath(style: LocalShellStyle, path: string): string {
  switch (style.kind) {
    case "powershell":
      return powershellQuote(path);
    case "cmd":
      return cmdQuote(path);
    case "wsl": {
      const share = wslSharePath(path, style.distro);
      if (share !== null) return bashQuote(share);
      const d = driveParts(path);
      return bashQuote(d ? `/mnt/${d.drive}/${d.rest}`.replace(/\/$/, "") || "/" : path);
    }
    case "msys": {
      const d = driveParts(path);
      return bashQuote(d ? `/${d.drive}/${d.rest}`.replace(/\/$/, "") : path);
    }
    default:
      return cmdQuote(path);
  }
}

/**
 * 拖进本机终端的多个文件：逐个改写后用空格连起来（不加回车）。
 * @param shellId 本机终端 ID
 * @param paths Windows 路径
 */
export function droppedText(shellId: string, paths: string[]): string {
  const style = shellStyle(shellId);
  return paths.map((p) => shellPath(style, p)).join(" ");
}

/**
 * 终端报告的当前目录对应的 Windows 文件夹（“在资源管理器中打开”用）。
 * @param shellId 本机终端 ID
 * @param cwd 终端报告的当前目录
 * @returns Windows 路径；无法对应时返回 null
 */
export function explorerFolder(shellId: string, cwd: string | null): string | null {
  if (!cwd) return null;
  const style = shellStyle(shellId);
  if (/^[A-Za-z]:([\\/]|$)/.test(cwd) || cwd.startsWith("\\\\")) return cwd.replace(/\//g, "\\");
  if (!cwd.startsWith("/")) return null;
  if (style.kind === "wsl") {
    const m = /^\/mnt\/([a-z])(\/.*)?$/i.exec(cwd);
    if (m) return `${m[1].toUpperCase()}:${(m[2] ?? "\\").replace(/\//g, "\\")}`;
    return `\\\\wsl.localhost\\${style.distro}${cwd.replace(/\//g, "\\")}`;
  }
  if (style.kind === "msys") {
    const m = /^\/([a-z])(\/.*)?$/i.exec(cwd);
    if (m) return `${m[1].toUpperCase()}:${(m[2] ?? "\\").replace(/\//g, "\\")}`;
  }
  return null;
}
