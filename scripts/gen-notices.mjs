// 生成随安装包分发的第三方许可证汇总（THIRD-PARTY-NOTICES.txt）。
// MIT、BSD、Apache 等许可证要求随软件附上版权声明和许可证原文，所以把实际打进程序的依赖逐个列出并附原文：
//   - Rust：从 pierlink 出发，沿“普通依赖”（不含开发、构建依赖）走 Windows 目标平台的依赖图；
//   - 前端：从 package.json 的 dependencies 出发，沿 node_modules 里各包的 dependencies 走（devDependencies 不打包）。
// 许可证原文相同的包合并成一段，减小文件。包里没带许可证文件的，用 scripts/third-party-licenses/ 里从上游仓库取回的原文；
// 两处都没有就报错退出（新加的依赖要先补上许可证原文）。
// MPL-2.0 组件按许可证要求写明源代码地址。
// 输出到 src-tauri/resources/licenses/（不提交到仓库，每次打包前重新生成）：
//   - THIRD-PARTY-NOTICES.txt：第三方组件汇总
//   - LibreSSL-LICENSE.txt：随 Win32-OpenSSH 分发的 libcrypto.dll（LibreSSL）的许可证
//   - Pierlink-LICENSE.txt：Pierlink 自己的许可证（英文正式文本 + 中文参考译文），安装器的许可证页也用它
// 用法：node scripts/gen-notices.mjs
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(root, "src-tauri", "resources", "licenses");
const outFile = join(outDir, "THIRD-PARTY-NOTICES.txt");
const fallbackDir = join(root, "scripts", "third-party-licenses");
/** 包里没带许可证文件时用的原文（取自上游仓库）：包名 → 文件名 */
const FALLBACK = {
  "alloc-stdlib": "alloc-stdlib.txt",
  pageant: "russh.txt",
  "russh-cryptovec": "russh.txt",
  "russh-util": "russh.txt",
  rusqlite_migration: "rusqlite_migration.txt",
  selectors: "selectors.txt",
  specta: "specta.txt",
  "specta-serde": "specta.txt",
  "specta-typescript": "specta.txt",
  "specta-util": "specta.txt",
  "webview2-com": "webview2-com.txt",
  "webview2-com-sys": "webview2-com.txt",
};
const TARGET = "x86_64-pc-windows-msvc";
/** 许可证文件名（不区分大小写，前缀匹配） */
const LICENSE_FILE = /^(licen[cs]e|copying|notice|copyright)([-_.].*)?$/i;
/** 单个许可证文件最大读取量，防止误读大文件 */
const MAX_LICENSE_BYTES = 256 * 1024;

function log(msg) {
  process.stdout.write(`${new Date().toISOString()} ${msg}\n`);
}

/** 读取目录里的许可证文件（按文件名排序），返回 [文件名, 内容] 列表 */
function licenseTexts(dir) {
  if (!existsSync(dir)) return [];
  return readdirSync(dir)
    .filter((f) => LICENSE_FILE.test(f))
    .sort()
    .flatMap((f) => {
      const p = join(dir, f);
      const st = statSync(p);
      if (!st.isFile() || st.size > MAX_LICENSE_BYTES) return [];
      return [[f, readFileSync(p, "utf8").replace(/\r\n/g, "\n").trim()]];
    });
}

/** Rust 依赖：pierlink 在 Windows 上实际链接进程序的包 */
function rustPackages() {
  const meta = JSON.parse(
    execFileSync("cargo", ["metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET], {
      cwd: root,
      maxBuffer: 256 * 1024 * 1024,
      encoding: "utf8",
    }),
  );
  const byId = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const start = meta.packages.find((p) => p.name === "pierlink");
  if (!start) throw new Error("cargo metadata 里找不到 pierlink 包");
  const members = new Set(meta.workspace_members);
  // 过程宏（syn、quote 等）只在编译时运行，它们和它们独有的依赖都不进入程序
  const procMacro = (p) => p.targets.every((t) => t.kind.includes("proc-macro"));
  const seen = new Set();
  const stack = [start.id];
  while (stack.length) {
    const id = stack.pop();
    if (seen.has(id) || procMacro(byId.get(id))) continue;
    seen.add(id);
    for (const d of nodes.get(id)?.deps ?? []) {
      // 只走普通依赖：开发依赖、构建脚本依赖不会进入程序
      if (d.dep_kinds.some((k) => k.kind === null)) stack.push(d.pkg);
    }
  }
  return [...seen]
    .filter((id) => !members.has(id))
    .map((id) => byId.get(id))
    .map((p) => ({ name: p.name, version: p.version, license: p.license ?? p.license_file ?? "未声明", texts: licenseTexts(dirname(p.manifest_path)) }));
}

/** 前端依赖：打进页面的包 */
function npmPackages() {
  const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
  const seen = new Map();
  const stack = Object.keys(pkg.dependencies ?? {}).map((name) => [name, root]);
  while (stack.length) {
    const [name, from] = stack.pop();
    // 按 Node 的查找规则：从引用方所在目录逐级向上找 node_modules
    let dir = null;
    for (let d = from; ; d = dirname(d)) {
      const cand = join(d, "node_modules", name);
      if (existsSync(join(cand, "package.json"))) { dir = cand; break; }
      if (dirname(d) === d) break;
    }
    if (!dir) throw new Error(`node_modules 里找不到依赖: 包=${name}, 引用方=${from}`);
    if (seen.has(dir)) continue;
    const p = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
    const license = typeof p.license === "string" ? p.license : p.license?.type ?? "未声明";
    seen.set(dir, { name: p.name, version: p.version, license, texts: licenseTexts(dir) });
    for (const dep of Object.keys(p.dependencies ?? {})) stack.push([dep, dir]);
  }
  return [...seen.values()];
}

/** 包里没带许可证文件时，用上游仓库的原文；返回 [文件名, 内容] 列表 */
function withFallback(p) {
  if (p.texts.length > 0 || !FALLBACK[p.name]) return p;
  const text = readFileSync(join(fallbackDir, FALLBACK[p.name]), "utf8").replace(/\r\n/g, "\n").trim();
  return { ...p, texts: [["LICENSE (from the upstream repository)", text]] };
}

/** 原文相同的包合并成一段 */
function render(title, packages) {
  const groups = new Map();
  const missing = [];
  for (const p of packages.map(withFallback).sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version))) {
    if (p.texts.length === 0) { missing.push(p); continue; }
    const body = p.texts.map(([f, t]) => `----- ${f} -----\n${t}`).join("\n\n");
    const key = createHash("sha256").update(body).digest("hex");
    if (!groups.has(key)) groups.set(key, { body, members: [] });
    groups.get(key).members.push(p);
  }
  const lines = [`${"=".repeat(78)}\n${title}（${packages.length} 个）\n${"=".repeat(78)}`];
  for (const g of groups.values()) {
    lines.push(`\n${g.members.map((p) => `${p.name} ${p.version} (${p.license})`).join("\n")}\n\n${g.body}\n`);
  }
  if (missing.length) {
    throw new Error(`这些包没有许可证原文，请把上游的许可证文件放进 scripts/third-party-licenses/ 并加到 FALLBACK: ${missing.map((p) => `${p.name} ${p.version} (${p.license})`).join(", ")}`);
  }
  return { text: lines.join("\n"), groups: groups.size };
}

async function main() {
  const rust = rustPackages();
  const npm = npmPackages();
  const r = render("Rust 组件 / Rust crates", rust);
  const n = render("前端组件 / Frontend packages", npm);
  const syncthing = JSON.parse(readFileSync(join(root, "src-tauri", "resources", "syncthing", "manifest.json"), "utf8")).version;
  const mpl = [
    ...rust.filter((p) => /MPL/.test(p.license)).map((p) => `  ${p.name} ${p.version}: https://crates.io/crates/${p.name}/${p.version}`),
    ...npm.filter((p) => /MPL/.test(p.license)).map((p) => `  ${p.name} ${p.version}: https://www.npmjs.com/package/${p.name}/v/${p.version}`),
  ];
  const header = "Pierlink 使用的第三方组件及其许可证\nThird-party components used by Pierlink and their licenses\n\n"
    + "Syncthing、Win32-OpenSSH、LibreSSL（Win32-OpenSSH 附带的 libcrypto.dll）的许可证见同目录下的单独文件。\n"
    + "Licenses for Syncthing, Win32-OpenSSH and LibreSSL (libcrypto.dll shipped with Win32-OpenSSH) are in separate files in this directory.\n\n"
    + "按 MPL-2.0 发布的组件，源代码可以从以下地址获取 / Source code of the MPL-2.0 licensed components is available at:\n"
    + `  Syncthing ${syncthing}: https://github.com/syncthing/syncthing/tree/${syncthing}\n`
    + `${mpl.join("\n")}\n\n`;
  await mkdir(outDir, { recursive: true });
  await writeFile(outFile, `${header}${r.text}\n\n${n.text}\n`);
  await writeFile(join(outDir, "LibreSSL-LICENSE.txt"), readFileSync(join(fallbackDir, "LibreSSL.txt")));
  // 安装器许可证页和安装目录里的 Pierlink 许可证：英文正式文本在前，中文参考译文在后
  // 转成纯文本：去掉标题井号、引用符号和链接语法（安装器许可证页原样显示）
  const plain = (md) => md.replace(/^#{1,6}\s+/gm, "").replace(/^>\s?/gm, "").replace(/\[([^\]]+)\]\([^)]+\)/g, "$1");
  const en = plain(readFileSync(join(root, "LICENSE"), "utf8").trim());
  const zh = plain(readFileSync(join(root, "LICENSE.zh-CN.md"), "utf8").trim());
  await writeFile(join(outDir, "Pierlink-LICENSE.txt"), `${en}\n\n${"-".repeat(70)}\n\n${zh}\n`);
  log(`生成许可证汇总: 路径=${outFile}, Rust 包=${rust.length}, 前端包=${npm.length}, 原文段数=${r.groups + n.groups}, MPL 组件=${mpl.length + 1}`);
}

main().catch((e) => {
  process.stderr.write(`生成许可证汇总失败: 原因=${e.message}\n`);
  process.exit(1);
});
