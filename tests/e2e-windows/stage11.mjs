// 0.9.5 命令预测的 Windows 端到端测试：服务器终端里敲过的命令，再输入开头时在光标后显示浅色预测，按 → 补全；
// 不在提示符下（cat 等输入、嵌套的 bash 里）不显示；本机 PowerShell 不显示；记录存在本机，重新打开 Pierlink 后还在；
// 可以只对一台服务器关掉；设置里能清除记录。
// 在 stage6.mjs 之后运行：那里已经添加了 WSL 里的 Linux 服务器 pl-linux（内置 SSH、指定私钥、指纹已信任）。
//
// 环境变量：PIERLINK_EXE、E2E_OUT（必填）；PIERLINK_TEST_LINUX_*（同 stage6.mjs，没有时服务器相关步骤失败）
// 每一步截图；某一步失败不中断后面的步骤。全部通过时退出码为 0。
import net from "node:net";
import { join } from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, openSync } from "node:fs";
import { killApp, launchApp, recorder, sleep } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, "0.9.5-predict");
const PORT = 9222;
if (!EXE || !OUT) throw new Error("缺少环境变量 PIERLINK_EXE 或 E2E_OUT");
mkdirSync(OUT, { recursive: true });
const LX = {
  host: process.env.PIERLINK_TEST_LINUX_HOST,
  port: process.env.PIERLINK_TEST_LINUX_PORT,
  user: process.env.PIERLINK_TEST_LINUX_USER,
  wsl: process.env.PIERLINK_TEST_LINUX_WSL,
};
const HAVE_LINUX = !!(LX.host && LX.port && LX.user && LX.wsl);
const LX_NAME = "pl-linux";

const rec = recorder(OUT);
killApp(EXE);
await sleep(1000);
let { p, u } = await launchApp(EXE, OUT, PORT);
const conn = () => p;

let wslSshd = null;
if (HAVE_LINUX) {
  const log = openSync(join(OUT, "wsl-sshd.log"), "a");
  wslSshd = spawn("wsl.exe", ["-d", LX.wsl, "-u", "root", "--", "/bin/sh", "-c", "mkdir -p /run/sshd && exec /usr/sbin/sshd -D -e"], { stdio: ["ignore", log, log] });
  const deadline = Date.now() + 30000;
  for (;;) {
    const up = await new Promise((r) => { const s = net.connect(Number(LX.port), LX.host); s.on("connect", () => { s.destroy(); r(true); }); s.on("error", () => r(false)); });
    if (up) break;
    if (Date.now() > deadline) { console.log("WSL 里的 sshd 30 秒内没有开始监听"); break; }
    await sleep(500);
  }
}

/** 当前可见窗格里终端的文字 */
const TERM = `([...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]?.querySelector('.xterm-rows')?.innerText||'')`;
/** 当前可见窗格里的浅色预测（没有时为 null） */
const GHOST = `([...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]?.querySelector('.term-ghost')?.textContent || null)`;
/** 终端最后一行非空文字 */
const LAST = `(${TERM}.split('\\n').map(l=>l.trimEnd()).filter(Boolean).pop()||'')`;
/** 最后一行是 bash 的空提示符 */
const AT_PROMPT = `/${LX.user}@[^\\s:]+:[^$]*\\$$/.test(${LAST})`;

async function clickVisible(selector) {
  const r = await p.js(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]; const e=s?.querySelector(${JSON.stringify(selector)}); if(!e) return null; const b=e.getBoundingClientRect(); return {x:b.x+b.width/2,y:b.y+b.height/2};})()`);
  if (!r) throw new Error(`可见窗格里找不到：${selector}`);
  await u.clickXY(r.x, r.y);
}

/** 服务器列表里 pl-linux 的行号 */
async function linuxRow() {
  return Number(await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes(${JSON.stringify(LX_NAME)})); return i>=0 ? String(i) : null;})()`, "服务器列表里有 pl-linux（stage6 添加的）"));
}

/** 新开一个 pl-linux 的终端标签，等到 bash 提示符并让终端拿到焦点 */
async function openLinux() {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器的环境变量");
  await u.clickText("Workspace", "header nav");
  await u.click(".srv-row", await linuxRow(), "left", 2);
  await u.waitFor(AT_PROMPT, "服务器终端出现 bash 提示符", 30000);
  await clickVisible(".xterm-screen");
  await sleep(300);
}

/** 把 pl-linux 的连接方式改成 label 对应的一项（同 stage7.mjs） */
async function setEngine(label) {
  await u.clickText("Workspace", "header nav");
  await u.click(".srv-row", await linuxRow(), "right");
  await u.clickText("Edit", ".pl-menu");
  await u.waitFor(`!!document.querySelector('.pl-dialog [role=tablist][aria-label="Connection"]')`, "编辑服务器对话框里的连接方式");
  await u.clickText(label, ".pl-dialog [role=tablist]");
  await u.waitFor(`(document.querySelector('.pl-dialog [role=tablist][aria-label="Connection"] [aria-selected=true]')?.textContent||'').startsWith(${JSON.stringify(label)})`, `连接方式选中“${label}”`);
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
}

/** 输入一段文字后等一会儿，确认没有出现预测 */
async function expectNoGhost(text, what) {
  await u.type(text);
  await sleep(2000);
  const g = await p.js(GHOST);
  if (g) throw new Error(`${what}不该有预测，实际显示：${g}`);
}

/** 清掉当前输入行（bash 的 Ctrl+U） */
const clearLine = () => u.key("u", "KeyU", 85, 2);

await rec.step(conn, "服务器终端：敲过的命令再输入开头时出现浅色预测，按 → 补全整条", async () => {
  await openLinux();
  await u.typeLine("echo pl-predict-alpha");
  await u.waitFor(`/^pl-predict-alpha\\s*$/m.test(${TERM})`, "echo 的输出", 10000);
  await u.waitFor(AT_PROMPT, "回到提示符");
  await u.type("echo pl-pre");
  const g = await u.waitFor(GHOST, "浅色预测", 10000);
  if (g !== "dict-alpha") throw new Error(`预测的内容不对：${g}`);
  await u.key("ArrowRight", "ArrowRight", 39);
  await u.waitFor(`${GHOST} === null && ${LAST}.endsWith('echo pl-predict-alpha')`, "→ 补全了整条", 5000);
  await u.enter();
  await u.waitFor(`(${TERM}.match(/^pl-predict-alpha\\s*$/gm)||[]).length >= 2`, "补全的命令执行了", 10000);
  return `预测：${g}`;
});

await rec.step(conn, "不在提示符下不显示预测：cat 等输入时、嵌套的 bash 里；回到原来的 shell 又有", async () => {
  await u.waitFor(AT_PROMPT, "在提示符下");
  await u.typeLine("cat");
  await sleep(500);
  await expectNoGhost("echo pl-pre", "cat 等输入时");
  await u.key("c", "KeyC", 67, 2);
  await u.waitFor(AT_PROMPT, "Ctrl+C 结束 cat 回到提示符", 10000);
  await u.typeLine("bash --norc");
  await u.waitFor(`/\\$$/.test(${LAST}) && !(${AT_PROMPT})`, "嵌套 bash 的提示符", 10000);
  await expectNoGhost("echo pl-pre", "嵌套的 bash 里");
  await clearLine();
  await u.typeLine("exit");
  await u.waitFor(AT_PROMPT, "退出嵌套的 bash", 10000);
  await u.type("echo pl-pre");
  const g = await u.waitFor(GHOST, "回到原来的 shell 后又有预测", 10000);
  await clearLine();
  return `回来后的预测：${g}`;
});

await rec.step(conn, "本机 PowerShell：没有 Pierlink 的预测（PowerShell 自己有）", async () => {
  await u.click('.tabbar button[aria-label="New terminal tab"]');
  await u.waitFor(`/PS [^>]*>/.test(${TERM})`, "PowerShell 提示符", 30000);
  await clickVisible(".xterm-screen");
  await u.typeLine("echo pl-ps-one");
  await u.waitFor(`/^pl-ps-one\\s*$/m.test(${TERM})`, "echo 的输出", 10000);
  await expectNoGhost("echo pl-ps", "本机 PowerShell 里");
  await u.esc();
});

// Git Bash 和系统 OpenSSH 的输出要经过 Windows 的 ConPTY 重新渲染：提示符标记能不能传过来只能在真机上看
await rec.step(conn, "本机 Git Bash（经过 ConPTY）：敲过的命令也有预测", async () => {
  await u.click(".pl-split__more");
  await u.clickText("Git Bash", ".pl-menu");
  await u.waitFor(`/\\$$/.test(${LAST})`, "Git Bash 提示符", 30000);
  await clickVisible(".xterm-screen");
  await u.typeLine("echo pl-gitbash-one");
  await u.waitFor(`/^pl-gitbash-one\\s*$/m.test(${TERM})`, "echo 的输出", 10000);
  await u.waitFor(`/\\$$/.test(${LAST})`, "回到提示符", 10000);
  await u.type("echo pl-gitb");
  const g = await u.waitFor(GHOST, "Git Bash 里的浅色预测", 10000);
  await clearLine();
  return `预测：${g}`;
});

await rec.step(conn, "系统 OpenSSH 连接（经过 ConPTY）：同一台服务器的记录也能预测；测完改回内置 SSH", async () => {
  await setEngine("System OpenSSH");
  try {
    await openLinux();
    await u.type("echo pl-pre");
    const g = await u.waitFor(GHOST, "系统 OpenSSH 连接里的浅色预测", 10000);
    await clearLine();
    return `预测：${g}`;
  } finally {
    await setEngine("Built-in SSH");
  }
});

await rec.step(conn, "重新打开 Pierlink：服务器的命令记录还在（存在本机）", async () => {
  p.close();
  killApp(EXE);
  await sleep(1000);
  ({ p, u } = await launchApp(EXE, OUT, PORT));
  await openLinux();
  await u.type("echo pl-pre");
  const g = await u.waitFor(GHOST, "重新打开后仍有预测", 15000);
  await clearLine();
  if (g !== "dict-alpha") throw new Error(`预测的内容不对：${g}`);
  return `预测：${g}`;
});

await rec.step(conn, "只对这台服务器关掉：右键菜单“关闭命令预测”后新开的终端没有预测；再开启", async () => {
  await u.clickText("Workspace", "header nav");
  await u.click(".srv-row", await linuxRow(), "right");
  await u.clickText("Turn off command prediction", ".pl-menu");
  await openLinux();
  await expectNoGhost("echo pl-pre", "关掉这台服务器的预测后");
  await clearLine();
  await u.click(".srv-row", await linuxRow(), "right");
  const items = await p.js(`[...document.querySelectorAll('.pl-menu [role=menuitem]')].map(x=>x.textContent.trim())`);
  if (!items.includes("Turn on command prediction")) throw new Error(`右键菜单里没有“开启命令预测”：${items.join(" / ")}`);
  await u.clickText("Turn on command prediction", ".pl-menu");
  await openLinux();
  await u.type("echo pl-pre");
  await u.waitFor(GHOST, "重新开启后又有预测", 10000);
  await clearLine();
});

await rec.step(conn, "设置里清除命令记录：之后连服务器也没有预测", async () => {
  await u.clickText("Settings", "nav");
  await u.clickText("Clear command history");
  await u.waitFor(`/Cleared/.test(document.querySelector('main')?.innerText||'')`, "显示已清除", 10000);
  await openLinux();
  await expectNoGhost("echo pl-pre", "清除记录后");
  await clearLine();
});

p.close();
killApp(EXE);
if (wslSshd) {
  try { execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "root", "--", "pkill", "-x", "sshd"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
  wslSshd.kill();
}
process.exitCode = rec.finish() ? 1 : 0;
