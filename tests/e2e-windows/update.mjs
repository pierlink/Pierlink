// 自动更新的真实流程（CI 的 update-windows 任务）：装好的“旧版”检查更新，发现测试用的新版本，
// 一键更新：下载、核对签名、正常关闭、安装、重新打开；装好的程序变成新版本，更新前的数据还在。
// 两个版本用同一份代码、这次运行临时生成的签名密钥打包，更新地址是本脚本起的本机 HTTP 服务（127.0.0.1:18765）。
//
// 参数：新版本文件所在目录（latest.json、安装包、签名）
// 环境变量：PIERLINK_EXE（装好的旧版）、E2E_OUT、UPDATE_TO（新版本号）
// 每步截图；全部通过时退出码为 0。
import http from "node:http";
import { createReadStream, existsSync, statSync } from "node:fs";
import { basename, join } from "node:path";
import { killApp, launchApp, pwsh, recorder, sleep } from "./lib.mjs";

const SERVE = process.argv[2];
const EXE = process.env.PIERLINK_EXE;
const TO = process.env.UPDATE_TO;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, "自动更新");
const PORT = 9222;
if (!SERVE || !EXE || !TO || !OUT) throw new Error("缺少参数（新版本目录）或环境变量 PIERLINK_EXE / UPDATE_TO / E2E_OUT");

// 本机更新服务：只发 SERVE 目录里的文件；记下每个请求，失败时写进结果
const requests = [];
const server = http.createServer((req, res) => {
  const name = basename(decodeURIComponent((req.url || "/").split("?")[0]));
  const file = join(SERVE, name);
  requests.push(`${req.method} ${name}`);
  if (!name || !existsSync(file)) { res.writeHead(404); res.end(); return; }
  res.writeHead(200, { "Content-Length": statSync(file).size });
  createReadStream(file).pipe(res);
});
await new Promise((r) => server.listen(18765, "127.0.0.1", r));

const rec = recorder(OUT);
killApp(EXE);
await sleep(1000);
let { p, u } = await launchApp(EXE, OUT, PORT);
/** 装好的 Pierlink.exe 的版本号（文件属性里的产品版本） */
const installedVersion = () => pwsh(`(Get-Item '${EXE.replace(/'/g, "''")}').VersionInfo.ProductVersion`).split("\n")[0].trim();
const SERVER_NAME = "更新前加的服务器";

await rec.step(p, "旧版：加一台服务器（只保存，不连接；更新后要还在）", async () => {
  await u.click('[aria-label="Add server"]');
  await u.waitFor(`!!document.querySelector('.pl-dialog')`, "添加服务器对话框");
  for (const [label, value] of [["Host", "192.0.2.20"], ["User", "pltest"], ["Display name", SERVER_NAME]]) {
    const ok = await p.js(`(()=>{const l=[...document.querySelectorAll('.pl-dialog label.pl-field')].find(l=>l.textContent.trim().startsWith(${JSON.stringify(label)})); const i=l?.querySelector('input'); if(!i) return false; i.focus(); i.select(); return true;})()`);
    if (!ok) throw new Error(`找不到输入框：${label}`);
    await u.type(value);
  }
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`[...document.querySelectorAll('.srv-row')].some(r=>r.innerText.includes(${JSON.stringify(SERVER_NAME)}))`, "服务器出现在列表里");
  return `旧版程序版本 ${installedVersion()}`;
});

await rec.step(p, "检查更新：发现测试用的新版本", async () => {
  await u.clickText("Settings", "nav");
  await u.waitFor(`[...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='Check for updates')`, "检查更新按钮");
  await u.clickText("Check for updates");
  const text = await u.waitFor(`(()=>{const m=document.body.innerText.match(/Pierlink [0-9.]+ is available|Couldn't check: [^\\n]+|You're on the latest version/); return m ? m[0] : null;})()`, "检查结果", 30000);
  if (!text.includes(TO)) throw new Error(`没有发现 ${TO}：${text}（请求：${requests.join("，") || "无"}）`);
  await u.waitFor(`[...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='Update now')`, "窗口底部的“立即更新”");
  return text;
});

await rec.step(() => p, "一键更新：下载、核对签名、关闭、安装、重新打开；装好的程序变成新版本", async () => {
  const before = installedVersion();
  await u.clickText("Update now");
  // 更新时 Pierlink 先正常关闭（同步、终端、隧道），安装程序装好后再打开
  const deadline = Date.now() + 240000;
  let now = before;
  let running = "0";
  while (Date.now() < deadline) {
    await sleep(3000);
    now = installedVersion() || now;
    running = pwsh("(Get-Process -Name Pierlink -ErrorAction SilentlyContinue | Measure-Object).Count").split("\n")[0].trim();
    if (now.startsWith(TO) && running !== "0") break;
  }
  if (!now.startsWith(TO)) throw new Error(`4 分钟内没有装好新版本：程序版本=${now}（更新前 ${before}；请求：${requests.join("，")}）`);
  if (running === "0") throw new Error("装好新版本后 Pierlink 没有重新打开");
  return `${before} → ${now}；请求：${requests.join("，")}`;
});

await rec.step(() => p, "更新后：设置页显示新版本，更新前加的服务器还在", async () => {
  try { p.close(); } catch { /* 旧连接已断 */ }
  killApp(EXE);
  await sleep(1500);
  ({ p, u } = await launchApp(EXE, OUT, PORT));
  await u.waitFor(`[...document.querySelectorAll('.srv-row')].some(r=>r.innerText.includes(${JSON.stringify(SERVER_NAME)}))`, "更新前加的服务器", 20000);
  await u.clickText("Settings", "nav");
  await u.waitFor(`document.body.innerText.includes(${JSON.stringify(TO)})`, `设置页显示版本 ${TO}`, 15000);
});

try { p.close(); } catch { /* 已断开 */ }
killApp(EXE);
server.close();
process.exitCode = rec.finish() ? 1 : 0;
