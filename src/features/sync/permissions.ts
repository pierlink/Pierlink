/** 失败原因是不是没有权限（Windows 的 Access is denied，Linux 的 permission denied 等；和后端的判断一致） */
export function isDenied(reason: string): boolean {
  return /access is denied|permission denied|operation not permitted/i.test(reason);
}

/** 给 bash 用的单引号括起来：里面的单引号写成 '\'' */
function shellQuote(s: string): string {
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

/**
 * 服务器上没有权限的文件 → 在那台服务器上修复的一行命令：把出问题的文件夹交给登录的用户，并让自己能读写。
 * 只取最上层的文件夹。要 sudo 密码，所以不在软件里执行，复制给用户到那台服务器的终端里运行。
 * @param remotePath 服务器上的同步目录
 * @param paths 失败的文件（相对同步目录，用 / 分隔）
 * @returns 命令；没有可用的路径时为空字符串
 */
export function serverFixCommand(remotePath: string, paths: string[]): string {
  const root = remotePath.replace(/\/+$/, "") || "/";
  const under = (dir: string) => (dir === "" ? root : root === "/" ? `/${dir}` : `${root}/${dir}`);
  const dirs = [...new Set(
    paths
      .filter((p) => p !== "" && !p.startsWith("/") && !p.split("/").includes(".."))
      .map((p) => under(p.includes("/") ? p.slice(0, p.lastIndexOf("/")) : "")),
  )].sort();
  // 只留最上层的：改上层时 -R 会连同下层一起
  const top = dirs.filter((d) => !dirs.some((o) => o !== d && (o === "/" || d.startsWith(`${o}/`))));
  if (top.length === 0) return "";
  const args = top.map(shellQuote).join(" ");
  return `sudo chown -R "$(id -un)": ${args} && sudo chmod -R u+rwX ${args}`;
}
