// 阶段 6 的 Windows 端到端测试：端口转发、广播输入、命令片段、资源概览。
// 在 run.mjs 之后运行：那里已经添加了“服务器”127.0.0.1（本机 Windows sshd，私钥登录）并信任了主机指纹。
//
// 环境变量：PIERLINK_EXE、E2E_OUT（必填）；PIERLINK_TEST_WIN_SSHD_KEY（没有时跳过服务器相关步骤）；
// PIERLINK_TEST_LINUX_HOST / _PORT / _USER / _KEY：CI 在 WSL 里起的 Linux 服务器（测资源概览、bash 下的广播和片段）
// PIERLINK_TEST_LINUX_PW_USER / _PW_PASS：同一台 Linux 服务器上只允许密码登录的用户（测 0.3.1“输一次密码，所有功能共用”）
// 每一步截图；某一步失败不中断后面的步骤。全部通过时退出码为 0。
import net from "node:net";
import { join } from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { openSync } from "node:fs";
import { killApp, launchApp, pwsh, recorder, sleep } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, "阶段6");
const HAVE_SSHD = !!process.env.PIERLINK_TEST_WIN_SSHD_KEY;
const PORT = 9222;
if (!EXE || !OUT) throw new Error("缺少环境变量 PIERLINK_EXE 或 E2E_OUT");

const rec = recorder(OUT);
killApp(EXE);
await sleep(1000);
let { p, u } = await launchApp(EXE, OUT, PORT);
const conn = () => p;

/** 本机找一个空闲端口 */
async function freePort() {
  return new Promise((resolve) => {
    const s = net.createServer();
    s.listen(0, "127.0.0.1", () => { const port = s.address().port; s.close(() => resolve(port)); });
  });
}

/** 连本机端口读服务器 sshd 的横幅；socks 为真时先做 SOCKS5 握手连 127.0.0.1:22 */
function readBanner(port, socks = false, timeoutMs = 10000) {
  return new Promise((resolve, reject) => {
    const s = net.connect(port, "127.0.0.1");
    let stage = socks ? 0 : 2;
    let buf = Buffer.alloc(0);
    const timer = setTimeout(() => { s.destroy(); reject(new Error(`读端口 ${port} 超时`)); }, timeoutMs);
    s.on("connect", () => { if (socks) s.write(Buffer.from([5, 1, 0])); });
    s.on("data", (d) => {
      buf = Buffer.concat([buf, d]);
      if (stage === 0 && buf.length >= 2) {
        if (buf[0] !== 5 || buf[1] !== 0) { clearTimeout(timer); s.destroy(); reject(new Error("SOCKS5 握手失败")); return; }
        buf = buf.subarray(2); stage = 1;
        s.write(Buffer.from([5, 1, 0, 1, 127, 0, 0, 1, 0, 22]));
      }
      if (stage === 1 && buf.length >= 10) {
        if (buf[1] !== 0) { clearTimeout(timer); s.destroy(); reject(new Error("SOCKS5 CONNECT 失败")); return; }
        buf = buf.subarray(10); stage = 2;
      }
      if (stage === 2 && buf.includes(0x0a)) { clearTimeout(timer); s.destroy(); resolve(buf.toString()); }
    });
    s.on("error", (e) => { clearTimeout(timer); reject(e); });
  });
}

/** 填对话框里的文本框（按标签开头匹配） */
async function fill(label, value) {
  const ok = await p.js(`(()=>{const l=[...document.querySelectorAll('.pl-dialog label.pl-field')].find(l=>l.firstChild?.textContent?.trim()===${JSON.stringify(label)} || l.textContent.trim().startsWith(${JSON.stringify(label)})); const i=l?.querySelector('input,textarea'); if(!i) return false; i.focus(); i.select(); return true;})()`);
  if (!ok) throw new Error(`找不到输入框：${label}`);
  await u.type(value);
}

/** 打开工具面板的某一页 */
async function panelPage(name) {
  if (!(await p.js(`!!document.querySelector('.ws__tool')`))) await u.click('[aria-label^="Tool panel"]');
  await u.waitFor(`!!document.querySelector('.ws__tool')`, "工具面板");
  await u.clickText(name, ".ws__tool-head");
  await sleep(300);
}

/** 新建端口转发（对话框里选类型、填字段），返回后等它出现在列表里 */
async function newForward({ kind, name, localPort, remotePort }) {
  await u.clickText("New", ".fwd");
  await u.waitFor(`!!document.querySelector('.pl-dialog')`, "新建端口转发对话框");
  if (kind !== "local") {
    await u.click(".pl-dialog .pl-select");
    await sleep(200);
    await u.clickText(kind === "remote" ? "Remote" : "SOCKS", ".pl-select__list");
    await sleep(200);
  }
  await fill("Name", name);
  if (kind === "remote") { await fill("Port on the server", String(remotePort)); await fill("Local target port", String(localPort)); }
  else await fill("Local port", String(localPort));
  if (kind === "local") await fill("Port", String(remotePort));
  await u.clickText("Create and start", ".pl-dialog");
}

/** 当前可见窗格里服务器概况的元素（JS 表达式），没有时为 null */
const OV = `([...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').map(s=>s.querySelector('.ov')).find(Boolean)||null)`;

/** 列表里某条转发的状态文字 */
const forwardState = (name) => p.js(`(()=>{const it=[...document.querySelectorAll('.fwd__item')].find(x=>x.querySelector('.fwd__name')?.textContent===${JSON.stringify(name)}); return it ? (it.querySelector('.fwd__state')?.textContent ?? 'off') : null;})()`);
const waitRunning = (name, ms = 30000) => u.waitFor(`(()=>{const it=[...document.querySelectorAll('.fwd__item')].find(x=>x.querySelector('.fwd__name')?.textContent===${JSON.stringify(name)}); return it && /^Running/.test(it.querySelector('.fwd__state')?.textContent||'');})()`, `转发“${name}”运行中`, ms);

const L_PORT = await freePort();
const S_PORT = await freePort();
const R_PORT = await freePort();
const target = net.createServer((s) => { s.end("pl-remote-ok\n"); });
await new Promise((r) => target.listen(0, "127.0.0.1", r));
const T_PORT = target.address().port;

// ---------- 端口转发 ----------
await rec.step(conn, "端口转发：本地、远程、SOCKS 三种都能用", async () => {
  if (!HAVE_SSHD) throw new Error("跳过：没有本机 sshd");
  // 侧栏选中 127.0.0.1（没有服务器窗格时面板跟随侧栏）
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes('127.0.0.1')); return i>=0 ? String(i) : null;})()`, "侧栏里的 127.0.0.1");
  await u.click(".srv-row", Number(row));
  await panelPage("Port forwarding");
  await newForward({ kind: "local", name: "pl-L", localPort: L_PORT, remotePort: 22 });
  await waitRunning("pl-L");
  const b1 = await readBanner(L_PORT);
  if (!b1.startsWith("SSH-")) throw new Error(`本地转发读到：${b1}`);
  await newForward({ kind: "dynamic", name: "pl-D", localPort: S_PORT });
  await waitRunning("pl-D");
  const b2 = await readBanner(S_PORT, true);
  if (!b2.startsWith("SSH-")) throw new Error(`SOCKS 读到：${b2}`);
  await newForward({ kind: "remote", name: "pl-R", localPort: T_PORT, remotePort: R_PORT });
  await waitRunning("pl-R");
  // “服务器”就是这台机器：连服务器上的 127.0.0.1:R_PORT，应当转到本机的 T_PORT
  const b3 = await readBanner(R_PORT);
  if (!b3.startsWith("pl-remote-ok")) throw new Error(`远程转发读到：${b3}`);
  return `本地 ${L_PORT}、SOCKS ${S_PORT}、远程 ${R_PORT}→${T_PORT}`;
});

await rec.step(conn, "端口被占用：提示里有占用的程序和进程号", async () => {
  if (!HAVE_SSHD) throw new Error("跳过：没有本机 sshd");
  const busy = net.createServer();
  const port = await freePort();
  await new Promise((r) => busy.listen(port, "127.0.0.1", r));
  try {
    await newForward({ kind: "local", name: "pl-busy", localPort: port, remotePort: 22 });
    // 端口冲突在后台建立连接时才发现：对话框正常关闭，错误显示在列表这一条的状态里（保存时同步发现的则在对话框里）
    const text = await u.waitFor(`(()=>{const t=[...document.querySelectorAll('.fwd__item .fwd__state, .pl-dialog .pl-banner, .fwd .pl-banner')].map(b=>b.innerText).join(' '); return /already used/.test(t) ? t : null;})()`, "端口被占用的提示", 20000);
    if (!/node/i.test(text) || !new RegExp(String(process.pid)).test(text)) throw new Error(`提示里没有 node.exe 和进程号 ${process.pid}：${text}`);
    await u.clickText("Cancel", ".pl-dialog").catch(() => {});
    return text.replace(/\s+/g, " ").slice(0, 160);
  } finally {
    busy.close();
  }
});

await rec.step(conn, "服务器那头断开连接后转发自动恢复，端口不变", async () => {
  if (!HAVE_SSHD) throw new Error("跳过：没有本机 sshd");
  // 内置 SSH 没有本机 ssh.exe 可杀；两种连接方式都从服务器那头断开：
  // 结束本机 sshd 服务为每条连接起的进程（服务本身的监听进程保留）
  const killed = Number(pwsh(`$svc = (Get-CimInstance Win32_Service -Filter "Name='sshd'").ProcessId; $p = @(Get-CimInstance Win32_Process | Where-Object { ($_.Name -eq 'sshd.exe' -and $_.ProcessId -ne $svc) -or $_.Name -eq 'sshd-session.exe' }); $p | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }; $p.Count`).split("\n")[0].trim());
  if (!(killed > 0)) throw new Error("没有找到转发用的 sshd 连接进程");
  await sleep(1500);
  await waitRunning("pl-L", 40000);
  await waitRunning("pl-D", 40000);
  const b = await readBanner(L_PORT);
  if (!b.startsWith("SSH-")) throw new Error(`恢复后本地转发读到：${b}`);
  return `结束了 ${killed} 个连接进程`;
});

await rec.step(conn, "重新打开应用：已启用的转发自动开启，停用的不开", async () => {
  if (!HAVE_SSHD) throw new Error("跳过：没有本机 sshd");
  // 停用 SOCKS
  await u.click(`.fwd__item:has(.fwd__name) [role=switch][aria-label="pl-D"]`);
  await u.waitFor(`document.querySelector('[role=switch][aria-label="pl-D"]').getAttribute('aria-checked')==='false'`, "SOCKS 已停用");
  p.close();
  killApp(EXE);
  await sleep(1500);
  ({ p, u } = await launchApp(EXE, OUT, PORT));
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes('127.0.0.1')); return i>=0 ? String(i) : null;})()`, "侧栏里的 127.0.0.1");
  await u.click(".srv-row", Number(row));
  await panelPage("Port forwarding");
  await waitRunning("pl-L", 40000);
  const b = await readBanner(L_PORT);
  if (!b.startsWith("SSH-")) throw new Error(`重启后本地转发读到：${b}`);
  const d = await forwardState("pl-D");
  if (!/^Stopped/.test(d ?? "")) throw new Error(`停用的 SOCKS 不应开启：${d}`);
  let socksUp = true;
  await readBanner(S_PORT, true, 2000).catch(() => { socksUp = false; });
  if (socksUp) throw new Error("停用的 SOCKS 端口还能连");
});

await rec.step(conn, "服务器概况：Windows 服务器的终端右上角显示不支持，不刷错误", async () => {
  if (!HAVE_SSHD) throw new Error("跳过：没有本机 sshd");
  await collapsePanel();
  // 概况在服务器终端右上角：打开这台服务器的终端
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes('127.0.0.1')); return i>=0 ? String(i) : null;})()`, "侧栏里的 127.0.0.1");
  await u.click(".srv-row", Number(row), "left", 2);
  await sleep(1500);
  if (await p.js(`/Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'')`)) await u.clickText("Trust and connect", ".pl-dialog");
  const text = await u.waitFor(`(()=>{const t=${OV}?.innerText||''; return /doesn't support|Couldn't read/.test(t) ? t : null;})()`, "概况的结论", 40000);
  if (!/doesn't support/.test(text)) throw new Error(`应当显示不支持：${text}`);
});

// ---------- 广播输入 ----------
/** 只在当前可见的窗格里按选择器找第 i 个元素并点击（隐藏标签的窗格也在页面里，只是不显示） */
/** 在第一个可见窗格里按 Ctrl+Shift+B 开广播；没生效（焦点还没到终端）时再点一次终端重按 */
async function startBroadcast(count) {
  const on = `!!document.querySelector('.broadcast-chip') && document.querySelectorAll('.pane--broadcast').length===${count}`;
  for (let attempt = 0; attempt < 3; attempt++) {
    await clickVisible(".xterm-screen", 0);
    await sleep(300);
    await u.key("B", "KeyB", 66, 2 | 8);
    if (await u.waitFor(on, "标签栏的广播标记和黄框窗格", 5000).then(() => true, () => false)) return;
  }
  await u.waitFor(on, "标签栏的广播标记和黄框窗格", 5000);
}
async function clickVisible(selector, i = 0) {
  // 只点元素落在自己窗格里的那部分：刚分屏时终端还没按新宽度重排，整块的中心可能落到隔壁窗格
  const r = await p.js(`(()=>{const v=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); const els=v.flatMap(s=>[...s.querySelectorAll(${JSON.stringify(selector)})].map(e=>[s,e])); const pair=els[${i}]; if(!pair) return null; const a=pair[1].getBoundingClientRect(), b=pair[0].getBoundingClientRect(); const l=Math.max(a.left,b.left), r=Math.min(a.right,b.right), t=Math.max(a.top,b.top), d=Math.min(a.bottom,b.bottom); if(r<=l||d<=t) return null; return {x:(l+r)/2,y:(t+d)/2};})()`);
  if (!r) throw new Error(`可见窗格里找不到：${selector}[${i}]`);
  await u.clickXY(r.x, r.y);
}
/** 当前可见窗格的终端文字 */
const paneTexts = () => p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').map(s=>s.querySelector('.xterm-rows')?.innerText||'')`);

await rec.step(conn, "广播输入：两个窗格同时收到，危险命令先确认，取消后不执行", async () => {
  // CI 屏幕只有 1024 宽，工具面板浮在终端上会挡住窗格工具栏：先收起
  if (await p.js(`!!document.querySelector('.ws__tool')`)) await u.click('[aria-label^="Tool panel"]');
  await u.waitFor(`!document.querySelector('.ws__tool')`, "工具面板已收起");
  // 新标签开两个本机终端（左右分屏）
  await u.click('.tabbar .pl-split__main');
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').some(s=>/PS /.test(s.querySelector('.xterm-rows')?.innerText||''))`, "第一个 PowerShell", 30000);
  await clickVisible(".pane__btn--split", 0);
  await u.waitFor(`!!document.querySelector('.pl-menu [role=menuitem]')`, "分屏菜单");
  await u.click(".pl-menu [role=menuitem]", 0);
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none' && /PS /.test(s.querySelector('.xterm-rows')?.innerText||'')).length===2`, "两个 PowerShell 窗格", 30000);
  await startBroadcast(2);
  await u.typeLine("echo pl-bcast-1");
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').every(s=>((s.querySelector('.xterm-rows')?.innerText||'').match(/^pl-bcast-1$/m)||[]).length===1)`, "两个窗格都输出 pl-bcast-1 各一次", 15000);
  // 危险命令
  await u.type("rm -rf pl-no-such-dir");
  await sleep(300);
  await u.enter();
  await u.waitFor(`/Run in all panes/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "危险命令确认框");
  await u.clickText("Cancel", ".pl-dialog");
  await sleep(1500);
  const texts = await paneTexts();
  // PowerShell 里 rm 是 Remove-Item 的别名：真执行了会报找不到路径或不认识 -rf 参数
  if (texts.some((t) => /Remove-Item|Cannot find path|parameter name 'rf'/i.test(t))) throw new Error("取消后命令仍然执行了");
  await u.key("c", "KeyC", 67, 2, "\x03");
  await sleep(500);
});

await rec.step(conn, "广播中出现密码提示的窗格自动退出广播", async () => {
  await u.typeLine("$null = Read-Host -AsSecureString Password");
  const notice = await u.waitFor(`document.querySelector('.broadcast-notice')?.innerText`, "退出广播的提示", 15000);
  if (!/password/i.test(notice)) throw new Error(`提示不对：${notice}`);
  // 两个窗格都在等密码，都退出了：广播结束
  await u.waitFor(`!document.querySelector('.broadcast-chip')`, "广播结束", 10000);
  // 结束 Read-Host
  for (let i = 0; i < 2; i++) {
    await clickVisible(".xterm-screen", i);
    await u.key("c", "KeyC", 67, 2, "\x03");
  }
  return notice;
});

// ---------- 命令片段 ----------
await rec.step(conn, "命令片段：变量填写后发送并执行；多行片段每行只执行一次", async () => {
  await clickVisible(".xterm-screen", 0);
  await panelPage("Snippets");
  // 含变量
  await u.clickText("New snippet", ".ws__tool");
  await fill("Name", "pl-var");
  await fill("Command", "echo pl-snip-{{who}}");
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "保存后对话框关闭");
  // 多行
  await u.clickText("New snippet", ".ws__tool");
  await fill("Name", "pl-multi");
  await fill("Command", "echo pl-m1\necho pl-m2");
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "保存后对话框关闭");
  const idx = async (title) => p.js(`[...document.querySelectorAll('.snip__item')].findIndex(x=>x.querySelector('.snip__title')?.textContent.startsWith(${JSON.stringify(title)}))`);
  await u.click(".snip__item .pl-btn--primary", await idx("pl-var"));
  await u.waitFor(`/Send “pl-var”/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "变量对话框");
  await u.type("abc");
  await u.clickText("Send and run", ".pl-dialog .pl-dialog__actions");
  await u.waitFor(`/^pl-snip-abc$/m.test(document.querySelectorAll('.pane-slot .xterm-rows')[0]?.innerText||'') || [...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').some(s=>/^pl-snip-abc$/m.test(s.querySelector('.xterm-rows')?.innerText||''))`, "终端输出 pl-snip-abc", 15000);
  await u.click(".snip__item .pl-btn--primary", await idx("pl-multi"));
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').some(s=>/^pl-m2$/m.test(s.querySelector('.xterm-rows')?.innerText||''))`, "终端输出 pl-m2", 15000);
  await sleep(1000);
  const texts = await paneTexts();
  const t = texts.find((x) => /^pl-m2$/m.test(x)) ?? "";
  const n1 = (t.match(/^pl-m1$/gm) || []).length;
  const n2 = (t.match(/^pl-m2$/gm) || []).length;
  if (n1 !== 1 || n2 !== 1) throw new Error(`多行片段执行次数不对：pl-m1=${n1}，pl-m2=${n2}`);
});

// ---------- Linux 服务器（CI 在 WSL 里起的 Ubuntu）----------
const LX = {
  host: process.env.PIERLINK_TEST_LINUX_HOST,
  port: process.env.PIERLINK_TEST_LINUX_PORT,
  user: process.env.PIERLINK_TEST_LINUX_USER,
  key: process.env.PIERLINK_TEST_LINUX_KEY,
};
const HAVE_LINUX = !!(LX.host && LX.port && LX.user && LX.key);
const LX_NAME = "pl-linux";
// WSL 里的 sshd 由本脚本启动、测完结束：CI 每一步结束时会结束那一步启动的进程。
// WSL 可能在两步之间重启过（/run 被清空），所以先建 sshd 需要的 /run/sshd
let wslSshd = null;
if (HAVE_LINUX && process.env.PIERLINK_TEST_LINUX_WSL) {
  const log = openSync(join(OUT, "wsl-sshd.log"), "a");
  wslSshd = spawn("wsl.exe", ["-d", process.env.PIERLINK_TEST_LINUX_WSL, "-u", "root", "--", "/bin/sh", "-c", "mkdir -p /run/sshd && exec /usr/sbin/sshd -D -e"], { stdio: ["ignore", log, log] });
  const deadline = Date.now() + 30000;
  for (;;) {
    const up = await new Promise((r) => { const s = net.connect(Number(LX.port), LX.host); s.on("connect", () => { s.destroy(); r(true); }); s.on("error", () => r(false)); });
    if (up) break;
    if (Date.now() > deadline) { console.log("WSL 里的 sshd 30 秒内没有开始监听"); break; }
    await sleep(500);
  }
}
/** 工具面板浮在终端上会挡住窗格工具栏：操作窗格前先收起 */
async function collapsePanel() {
  if (await p.js(`!!document.querySelector('.ws__tool')`)) await u.click('[aria-label^="Tool panel"]');
  await u.waitFor(`!document.querySelector('.ws__tool')`, "工具面板已收起");
}
/** 可见窗格里出现 bash 提示符（pl@主机名:~$）的个数 */
const bashPanes = `[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none' && /${LX.user}@[^\\s:]+:~\\$/.test(s.querySelector('.xterm-rows')?.innerText||'')).length`;
/** 每个可见窗格里整行等于 line 的次数 */
const lineCounts = (line) => p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').map(s=>((s.querySelector('.xterm-rows')?.innerText||'').match(/^${line}$/gm)||[]).length)`);

await rec.step(conn, "Linux 服务器（WSL Ubuntu）：添加、信任指纹、连接，出现 bash 提示符", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器的环境变量（CI 里装 WSL 的步骤失败了？）");
  await collapsePanel();
  await u.click('[aria-label="Add server"]');
  await u.waitFor(`!!document.querySelector('.pl-dialog')`, "添加服务器对话框");
  await fill("Host", LX.host);
  await fill("Port", LX.port);
  await fill("User", LX.user);
  await fill("Display name", LX_NAME);
  await u.clickText("Specific key", ".pl-dialog");
  await fill("Private key file", LX.key);
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes(${JSON.stringify(LX_NAME)})); return i>=0 ? String(i) : null;})()`, "服务器出现在列表里");
  await u.click(".srv-row", Number(row), "left", 2);
  await u.waitFor(`/Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "主机指纹确认", 30000);
  await u.clickText("Trust and connect", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "连接对话框关闭", 30000);
  await u.waitFor(`${bashPanes} >= 1`, "服务器终端出现 bash 提示符", 30000);
  return (await u.termText()).split("\n").filter(Boolean).slice(-2).join(" / ");
});

await rec.step(conn, "服务器概况：Linux 服务器终端右上角显示运行时间、系统名和 CPU、内存、每个挂载点的磁盘、网速", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器");
  // 第一次采样没有 CPU 占用（要两次采样相减），1 秒后第二次采样才有
  const info = await u.waitFor(`(()=>{const s=${OV}; if(!s) return null; const m=[...s.querySelectorAll('[role=meter]')].map(x=>({label:x.getAttribute('aria-label'), now:x.getAttribute('aria-valuenow'), detail:x.closest('.ov-meter, .ov-disk')?.getAttribute('title')||''})); const cpu=m.find(x=>x.label==='CPU'); if(!cpu || cpu.now===null) return null; return {meters:m, summary:s.querySelector('.ov__summary')?.innerText||'', text:s.innerText};})()`, "CPU 格有数值（第二次采样后）", 60000);
  if (/doesn't support|Couldn't read|retrying/i.test(info.text)) throw new Error(`资源概览报错：${info.text}`);
  const find = (re) => info.meters.find((m) => re.test(m.label));
  const cpu = find(/^CPU$/), mem = find(/^Memory$/), disk = find(/^Disk \//);
  const pct = (m) => (m && m.now !== null ? Number(m.now) : NaN);
  if (!(pct(cpu) >= 0 && pct(cpu) <= 100)) throw new Error(`CPU 数值不对：${JSON.stringify(cpu)}`);
  if (!(pct(mem) > 0 && pct(mem) < 100)) throw new Error(`内存数值不对：${JSON.stringify(mem)}`);
  // 交换区：开了就有百分比，没开显示“Swap is off”
  const swap = find(/^Swap$/);
  if (!swap || !(swap.now === null ? /Swap is off/.test(swap.detail) : pct(swap) >= 0 && pct(swap) <= 100)) throw new Error(`交换卡片不对：${JSON.stringify(info.meters)}`);
  // 百分比按 df 的算法（不含给 root 预留的空间）：几乎空的盘可以是 0%
  if (!disk || !(pct(disk) >= 0 && pct(disk) < 100) || !/ used, .+ free/.test(disk.detail)) throw new Error(`没有根目录磁盘条或数值不对：${JSON.stringify(info.meters)}`);
  if (!/↓ .+\/s +↑ .+\/s/.test(info.text)) throw new Error(`第二次采样后应显示网速：${info.text}`);
  if (!/Ubuntu/.test(info.summary) || !/^Up /.test(info.summary)) throw new Error(`摘要里没有运行时间或系统名：${info.summary}`);
  return `${info.summary}｜CPU ${cpu.now}%（${cpu.detail}）｜内存 ${mem.now}%（${mem.detail}）｜${disk.label} ${disk.now}%（${disk.detail}）`;
});

await rec.step(conn, "bash 下的广播：两格同时收到；片段发到两个窗格各执行一次；sudo 密码提示让窗格退出广播", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器");
  await collapsePanel();
  // 右边再开一个同一台服务器的 bash
  await clickVisible(".pane__btn--split", 0);
  await u.waitFor(`/${LX_NAME}/.test(document.querySelector('.pl-menu [role=menuitem]')?.textContent||'')`, "分屏菜单第一项是同一台服务器");
  await u.click(".pl-menu [role=menuitem]", 0);
  await u.waitFor(`${bashPanes} === 2`, "两个 bash 窗格", 30000);
  await startBroadcast(2);
  // 命令回显是 $((40+2))，只有真的执行了才会出现 pl-lx-42 这一整行
  await u.typeLine("echo pl-lx-$((40+2))");
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').every(s=>((s.querySelector('.xterm-rows')?.innerText||'').match(/^pl-lx-42$/gm)||[]).length===1)`, "两个 bash 都输出 pl-lx-42 各一次", 15000);
  // 片段：广播中先确认，发到两个窗格
  await panelPage("Snippets");
  await u.waitFor(`/2 panes/.test(document.querySelector('.ws__tool')?.innerText||'')`, "片段页提示会发到 2 个窗格");
  const idx = await p.js(`[...document.querySelectorAll('.snip__item')].findIndex(x=>x.querySelector('.snip__title')?.textContent.startsWith('pl-multi'))`);
  if (idx < 0) throw new Error("找不到前面建的多行片段 pl-multi");
  await u.click(".snip__item .pl-btn--primary", idx);
  await u.waitFor(`/2 broadcast panes/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "确认框写明发到 2 个广播窗格");
  await u.clickText("Send and run", ".pl-dialog .pl-dialog__actions");
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').every(s=>/^pl-m2$/m.test(s.querySelector('.xterm-rows')?.innerText||''))`, "两个 bash 都输出 pl-m2", 15000);
  await sleep(1000);
  const m1 = await lineCounts("pl-m1"), m2 = await lineCounts("pl-m2");
  if (m1.some((n) => n !== 1) || m2.some((n) => n !== 1)) throw new Error(`多行片段执行次数不对：pl-m1=${m1}，pl-m2=${m2}`);
  // sudo：两个窗格都出现 [sudo] password for pl:，都退出广播，广播结束
  await collapsePanel();
  await clickVisible(".xterm-screen", 0);
  await u.typeLine("sudo -k; sudo true");
  const notice = await u.waitFor(`document.querySelector('.broadcast-notice')?.innerText`, "退出广播的提示", 15000);
  if (!/password/i.test(notice)) throw new Error(`提示不对：${notice}`);
  await u.waitFor(`!document.querySelector('.broadcast-chip')`, "两个窗格都在等密码：广播结束", 10000);
  const prompts = await p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').map(s=>/\\[sudo\\] password for ${LX.user}/.test(s.querySelector('.xterm-rows')?.innerText||''))`);
  if (!prompts.every(Boolean)) throw new Error(`不是每个窗格都出现了 sudo 密码提示：${prompts}`);
  for (let i = 0; i < 2; i++) {
    await clickVisible(".xterm-screen", i);
    await u.key("c", "KeyC", 67, 2, "\x03");
  }
  return `pl-lx-42 各一次；片段 pl-m1=${m1} pl-m2=${m2}；${notice.replace(/\s+/g, " ")}`;
});

// ---------- 密码服务器：连接时输一次密码，终端、资源概览、端口转发一起用（0.3.1）----------
const PW = { user: process.env.PIERLINK_TEST_LINUX_PW_USER, pass: process.env.PIERLINK_TEST_LINUX_PW_PASS };
const HAVE_PW = HAVE_LINUX && !!(PW.user && PW.pass);
const PW_NAME = "pl-password";
const PW_L_PORT = await freePort();
/** 可见窗格里出现密码用户的 bash 提示符 */
const pwPrompt = `[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none' && /${PW.user}@[^\\s:]+:~\\$/.test(s.querySelector('.xterm-rows')?.innerText||'')).length`;
/** 等全局密码框出现，返回框里的文字 */
const waitPasswordDialog = (ms = 30000) => u.waitFor(`(()=>{const d=[...document.querySelectorAll('.pl-dialog')].find(x=>x.querySelector('.pw-form')); return d ? d.innerText : null;})()`, "密码框", ms);
/** 有没有打开着的密码框 */
const passwordDialogOpen = () => p.js(`!!document.querySelector('.pw-form')`);
/** 在密码框里输入并登录；remember=false 时先取消“记住密码” */
async function answerPassword(pass, remember) {
  await p.js(`document.querySelector('.pw-form input[type=password]')?.focus()`);
  await u.type(pass);
  if (!remember) await p.js(`(()=>{const c=document.querySelector('.pw-form input[type=checkbox]'); if(c && c.checked) c.click();})()`);
  await u.clickText("Log in", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pw-form')`, "密码框关闭", 15000);
}
/** 选中侧栏里的某台服务器 */
async function selectServer(name) {
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes(${JSON.stringify(name)})); return i>=0 ? String(i) : null;})()`, `侧栏里的 ${name}`);
  await u.click(".srv-row", Number(row));
  return Number(row);
}

await rec.step(conn, "密码服务器：输错密码会提示并重新问；输对后终端、资源概览、端口转发都不再要密码", async () => {
  if (!HAVE_PW) throw new Error("没有 WSL 里的密码登录用户（PIERLINK_TEST_LINUX_PW_USER / _PW_PASS）");
  await collapsePanel();
  await u.click('[aria-label="Add server"]');
  await u.waitFor(`!!document.querySelector('.pl-dialog')`, "添加服务器对话框");
  await fill("Host", LX.host);
  await fill("Port", LX.port);
  await fill("User", PW.user);
  await fill("Display name", PW_NAME);
  // 选“密码”登录，但不在这里保存密码：连接时再输
  await u.clickText("Password", ".pl-dialog");
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
  const row = await selectServer(PW_NAME);
  await u.click(".srv-row", row, "left", 2);
  // 同一台服务器（localhost:2222）的指纹前面已经信任过；万一再问就信任
  await u.waitFor(`!!document.querySelector('.pw-form') || /Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "密码框或指纹确认", 30000);
  if (!(await passwordDialogOpen())) {
    await u.clickText("Trust and connect", ".pl-dialog");
  }
  const first = await waitPasswordDialog();
  if (!/Password for pl-password/.test(first) || !/uses this password/.test(first)) throw new Error(`密码框文字不对：${first}`);
  // 先输错：终端提示密码不对，Pierlink 忘掉它
  await answerPassword("pl-wrong-password", false);
  const ended = await u.waitFor(`(()=>{const e=[...document.querySelectorAll('.term-ended')].find(x=>x.offsetParent); return e ? e.innerText : null;})()`, "登录失败的提示", 30000);
  if (!/Login failed/.test(ended) || !/wrong or has changed/.test(ended)) throw new Error(`输错密码后的提示不对：${ended}`);
  // 重新连接：会再问（错的密码已作废，不会被再用一次）
  await u.clickText("Reconnect", ".term-ended");
  await waitPasswordDialog();
  await answerPassword(PW.pass, false);
  await u.waitFor(`${pwPrompt} >= 1`, "密码用户的 bash 提示符", 30000);
  // 服务器概况：后台连接自动用同一个密码
  const summary = await u.waitFor(`(()=>{const s=${OV}; if(!s) return null; const t=s.innerText; if(/Couldn't read|password is needed/.test(t)) return 'ERR:'+t; return s.querySelector('.ov-meters') ? s.querySelector('.ov__summary')?.innerText||null : null;})()`, "概况出数据", 40000);
  if (summary.startsWith("ERR:")) throw new Error(`概况没用上密码：${summary}`);
  await panelPage("Port forwarding");
  // 端口转发（经服务器连回它自己的 sshd）：也自动用同一个密码
  await newForward({ kind: "local", name: "pl-pw-L", localPort: PW_L_PORT, remotePort: Number(LX.port) });
  await waitRunning("pl-pw-L", 40000);
  const b = await readBanner(PW_L_PORT);
  if (!b.startsWith("SSH-")) throw new Error(`经密码服务器的本地转发读到：${b}`);
  if (await passwordDialogOpen()) throw new Error("资源概览或端口转发又弹了密码框");
  return `${summary}｜转发读到 ${b.trim()}`;
});

await rec.step(conn, "重新打开 Pierlink（没记住密码）：转发等待输入密码，输入一次后自动恢复", async () => {
  if (!HAVE_PW) throw new Error("没有 WSL 里的密码登录用户");
  p.close();
  killApp(EXE);
  await sleep(1500);
  ({ p, u } = await launchApp(EXE, OUT, PORT));
  await selectServer(PW_NAME);
  await panelPage("Port forwarding");
  await u.waitFor(`/^Waiting for password/.test((()=>{const it=[...document.querySelectorAll('.fwd__item')].find(x=>x.querySelector('.fwd__name')?.textContent==='pl-pw-L'); return it?.querySelector('.fwd__state')?.textContent||'';})())`, "转发显示等待输入密码", 40000);
  // 等待期间不该自己弹框（只有用户点了才问）
  if (await passwordDialogOpen()) throw new Error("没点“输入密码”就弹了密码框");
  await u.clickText("Enter password", ".fwd");
  const text = await waitPasswordDialog();
  if (!/Password for pl-password/.test(text)) throw new Error(`密码框文字不对：${text}`);
  await answerPassword(PW.pass, false);
  await waitRunning("pl-pw-L", 40000);
  const b = await readBanner(PW_L_PORT);
  if (!b.startsWith("SSH-")) throw new Error(`输入密码后本地转发读到：${b}`);
  return `转发读到 ${b.trim()}`;
});

p.close();
target.close();
killApp(EXE);
if (wslSshd) {
  try { execFileSync("wsl.exe", ["-d", process.env.PIERLINK_TEST_LINUX_WSL, "-u", "root", "--", "pkill", "-x", "sshd"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
  wslSshd.kill();
}
process.exitCode = rec.finish() ? 1 : 0;
