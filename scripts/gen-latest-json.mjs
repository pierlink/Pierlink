// 生成自动更新清单 latest.json（和安装包、签名一起上传到 GitHub Releases）。
// 读取：tauri.conf.json 的版本号、打包出的安装包和 .sig 签名、release-notes/<版本>.md（没有就留空）。
// 输出：target/release/bundle/nsis/latest.json
import { readFileSync, readdirSync, writeFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const root = new URL("..", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");
const log = (msg) => console.log(`[latest.json] ${msg}`);

const version = JSON.parse(readFileSync(join(root, "src-tauri", "tauri.conf.json"), "utf8")).version;
const dir = join(root, "target", "release", "bundle", "nsis");
const exe = readdirSync(dir).find((f) => f.endsWith("-setup.exe"));
if (!exe) throw new Error(`没有找到安装包：目录=${dir}`);
const sigPath = join(dir, `${exe}.sig`);
if (!existsSync(sigPath)) throw new Error(`没有找到签名文件：${sigPath}（打包时没有设签名密钥？）`);
const notesPath = join(root, "release-notes", `${version}.md`);
const notes = existsSync(notesPath) ? readFileSync(notesPath, "utf8").trim() : "";

const manifest = {
  version,
  notes,
  pub_date: new Date().toISOString(),
  platforms: {
    "windows-x86_64": {
      signature: readFileSync(sigPath, "utf8").trim(),
      url: `https://github.com/pierlink/Pierlink/releases/download/v${version}/${encodeURIComponent(exe)}`,
    },
  },
};
writeFileSync(join(dir, "latest.json"), JSON.stringify(manifest, null, 2) + "\n");
log(`已生成: 版本=${version}, 安装包=${exe}, 说明=${notes ? "有" : "无"}`);
