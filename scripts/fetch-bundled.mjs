// 下载并准备随安装包分发的第三方程序：
//   - Syncthing：本机用的 windows-amd64 syncthing.exe；上传到服务器用的 linux-amd64 / linux-arm64 程序（预先 gzip，减少上传量）
//   - Win32-OpenSSH 客户端：ssh.exe、ssh-keygen.exe 及其依赖的 libcrypto.dll（设计第 0 节“SSH 程序”）
//   - 以上组件的许可证文件，放到 licenses/ 下随安装包分发
// 版本和官方压缩包的 SHA-256 固定写在下面，下载后逐个核对，不一致立即失败。
// 输出到 src-tauri/resources/（不提交到仓库），syncthing/manifest.json 记录解压后程序的 SHA-256。
// 用法：node scripts/fetch-bundled.mjs
import { createHash } from "node:crypto";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { gunzipSync, gzipSync, inflateRawSync } from "node:zlib";

const VERSION = "v2.1.7";
const BASE = `https://github.com/syncthing/syncthing/releases/download/${VERSION}`;
/** 官方 sha256sum.txt 中的值 */
const PACKAGES = {
  "linux-amd64": { file: `syncthing-linux-amd64-${VERSION}.tar.gz`, sha256: "2edc416e7ed136b7c858ed5f3f02a3b8316589605e54c380228ceea7acbf0142" },
  "linux-arm64": { file: `syncthing-linux-arm64-${VERSION}.tar.gz`, sha256: "48e456690cd2b97e7d26b02b5ac35f62a3b36172267b144cfeaf02c993ab955c" },
  "windows-amd64": { file: `syncthing-windows-amd64-${VERSION}.zip`, sha256: "3ce7da770f546a8d02b65b2e7ee31f2a3b8b7c3f990ede045590cca4172ecfe1" },
};

/** Win32-OpenSSH：选用发布满一年的稳定版本；SHA-256 为 GitHub 发布页给出的值 */
const OPENSSH = {
  version: "10.0.0.0p2-Preview",
  url: "https://github.com/PowerShell/Win32-OpenSSH/releases/download/10.0.0.0p2-Preview/OpenSSH-Win64.zip",
  file: "OpenSSH-Win64-10.0.0.0p2.zip",
  sha256: "23f50f3458c4c5d0b12217c6a5ddfde0137210a30fa870e98b29827f7b43aba5",
  /** 只带客户端需要的文件 */
  files: ["ssh.exe", "ssh-keygen.exe", "libcrypto.dll"],
};

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const resDir = join(root, "src-tauri", "resources");
const outDir = join(resDir, "syncthing");
const sshDir = join(resDir, "openssh");
const licDir = join(resDir, "licenses");
const cacheDir = join(root, ".cache-downloads");

const sha256 = (buf) => createHash("sha256").update(buf).digest("hex");

function log(msg) {
  process.stdout.write(`${new Date().toISOString()} ${msg}\n`);
}

/** 下载（有缓存且校验通过就直接用缓存）；url 省略时从 Syncthing 发布页下载 */
async function download(name, expected, url = `${BASE}/${name}`) {
  await mkdir(cacheDir, { recursive: true });
  const cached = join(cacheDir, name);
  if (existsSync(cached)) {
    const buf = await readFile(cached);
    if (sha256(buf) === expected) {
      log(`使用缓存: 文件=${name}`);
      return buf;
    }
  }
  log(`下载: 文件=${name}`);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`下载失败: 文件=${name}, 状态=${res.status}`);
  const buf = Buffer.from(await res.arrayBuffer());
  const actual = sha256(buf);
  if (actual !== expected) throw new Error(`校验失败: 文件=${name}, 期望=${expected}, 实际=${actual}`);
  await writeFile(`${cached}.part`, buf);
  await rename(`${cached}.part`, cached);
  return buf;
}

/** 从 tar.gz 里取出路径正好是 <path> 的普通文件（包里还有同名的启动脚本等，必须按完整路径匹配） */
function extractFromTarGz(gz, name) {
  const tar = gunzipSync(gz);
  for (let off = 0; off + 512 <= tar.length; ) {
    const header = tar.subarray(off, off + 512);
    if (header.every((b) => b === 0)) break;
    const field = (start, len) => header.subarray(start, start + len).toString("utf8").replace(/\0.*$/s, "");
    const prefix = field(345, 155);
    const path = prefix ? `${prefix}/${field(0, 100)}` : field(0, 100);
    const size = parseInt(field(124, 12).trim() || "0", 8);
    const type = String.fromCharCode(header[156] || 48);
    const body = off + 512;
    if ((type === "0" || type === "\0") && path === name) return Buffer.from(tar.subarray(body, body + size));
    off = body + Math.ceil(size / 512) * 512;
  }
  throw new Error(`压缩包里找不到: 文件=${name}`);
}

/** 从 zip 里取出路径正好是 <path> 的文件（按中央目录读取，支持存储和 deflate） */
function extractFromZip(zip, name) {
  const eocd = zip.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  if (eocd < 0) throw new Error("不是有效的 zip 文件");
  const count = zip.readUInt16LE(eocd + 10);
  let p = zip.readUInt32LE(eocd + 16);
  for (let i = 0; i < count; i++) {
    if (zip.readUInt32LE(p) !== 0x02014b50) throw new Error("zip 中央目录损坏");
    const method = zip.readUInt16LE(p + 10);
    const compSize = zip.readUInt32LE(p + 20);
    const nameLen = zip.readUInt16LE(p + 28);
    const extraLen = zip.readUInt16LE(p + 30);
    const commentLen = zip.readUInt16LE(p + 32);
    const local = zip.readUInt32LE(p + 42);
    const path = zip.subarray(p + 46, p + 46 + nameLen).toString("utf8");
    if (path === name) {
      const lNameLen = zip.readUInt16LE(local + 26);
      const lExtraLen = zip.readUInt16LE(local + 28);
      const data = zip.subarray(local + 30 + lNameLen + lExtraLen, local + 30 + lNameLen + lExtraLen + compSize);
      if (method === 0) return Buffer.from(data);
      if (method === 8) return inflateRawSync(data);
      throw new Error(`不支持的 zip 压缩方式: 方式=${method}`);
    }
    p += 46 + nameLen + extraLen + commentLen;
  }
  throw new Error(`压缩包里找不到: 文件=${name}`);
}

/** 压缩包的顶层目录名（与文件名去掉扩展名相同） */
function topDir(file) {
  return file.replace(/\.(tar\.gz|zip)$/, "");
}

/** 程序应在 10 MB 以上；取错文件时（如同名的小脚本）立即失败 */
function checkSize(buf, from) {
  if (buf.length < 10 * 1024 * 1024) throw new Error(`取出的程序大小异常: 来源=${from}, 大小=${buf.length}`);
  return buf;
}

async function main() {
  await mkdir(outDir, { recursive: true });
  const manifest = { version: VERSION, linux: {}, windows: {} };

  for (const arch of ["amd64", "arm64"]) {
    const pkg = PACKAGES[`linux-${arch}`];
    const bin = checkSize(extractFromTarGz(await download(pkg.file, pkg.sha256), `${topDir(pkg.file)}/syncthing`), pkg.file);
    const file = `syncthing-linux-${arch}.gz`;
    await writeFile(join(outDir, file), gzipSync(bin, { level: 9 }));
    manifest.linux[arch] = { file, sha256: sha256(bin), size: bin.length };
    log(`准备完成: 文件=${file}, 原始大小=${bin.length}`);
  }

  const win = PACKAGES["windows-amd64"];
  const exe = checkSize(extractFromZip(await download(win.file, win.sha256), `${topDir(win.file)}/syncthing.exe`), win.file);
  await writeFile(join(outDir, "syncthing.exe"), exe);
  manifest.windows.amd64 = { file: "syncthing.exe", sha256: sha256(exe), size: exe.length };
  log(`准备完成: 文件=syncthing.exe, 大小=${exe.length}`);

  await writeFile(join(outDir, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  log(`写入清单: 路径=${join(outDir, "manifest.json")}`);

  // Win32-OpenSSH 客户端
  await mkdir(sshDir, { recursive: true });
  await mkdir(licDir, { recursive: true });
  const sshZip = await download(OPENSSH.file, OPENSSH.sha256, OPENSSH.url);
  for (const f of OPENSSH.files) {
    const buf = extractFromZip(sshZip, `OpenSSH-Win64/${f}`);
    if (buf.length < 100 * 1024) throw new Error(`取出的文件大小异常: 文件=${f}, 大小=${buf.length}`);
    await writeFile(join(sshDir, f), buf);
    log(`准备完成: 文件=openssh/${f}, 大小=${buf.length}`);
  }

  // 许可证
  const licenses = [
    ["Win32-OpenSSH-LICENSE.txt", extractFromZip(sshZip, "OpenSSH-Win64/LICENSE.txt")],
    ["Win32-OpenSSH-NOTICE.txt", extractFromZip(sshZip, "OpenSSH-Win64/NOTICE.txt")],
    ["Syncthing-LICENSE.txt", extractFromZip(await download(win.file, win.sha256), `${topDir(win.file)}/LICENSE.txt`)],
    ["Syncthing-AUTHORS.txt", extractFromZip(await download(win.file, win.sha256), `${topDir(win.file)}/AUTHORS.txt`)],
  ];
  for (const [name, buf] of licenses) {
    await writeFile(join(licDir, name), buf);
    log(`写入许可证: 文件=licenses/${name}, 大小=${buf.length}`);
  }
}

main().catch((e) => {
  process.stderr.write(`准备随包程序失败: 原因=${e.message}\n`);
  process.exit(1);
});
