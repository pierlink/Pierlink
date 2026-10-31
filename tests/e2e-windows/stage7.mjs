// 0.4.0 的 Windows 端到端测试：要验证码的服务器登录一次后所有功能共用；连接方式（内置 SSH / 系统 OpenSSH）的默认值和切换；
// 从 ssh config 导入的服务器转换成内置 SSH；检查更新。
// 在 stage6.mjs 之后运行：那里已经添加了 WSL 里的 Linux 服务器 pl-linux（密钥登录）并信任了指纹。
// 其他页面（终端、文件、资源概览、转发、密码只输一次）在 run.mjs、stage6.mjs 里已经按默认的内置 SSH 跑过。
//
// 环境变量：PIERLINK_EXE、E2E_OUT（必填）；PIERLINK_TEST_LINUX_*（同 stage6.mjs，没有时跳过服务器相关步骤）；
// PIERLINK_TEST_LINUX_OTP_USER / _OTP_PASS / _OTP_SECRET：同一台服务器上要密码加验证码（TOTP）的用户
// 每一步截图；某一步失败不中断后面的步骤。全部通过时退出码为 0。
import net from "node:net";
import { join } from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, openSync, readFileSync, writeFileSync } from "node:fs";
import { createHmac } from "node:crypto";
import { killApp, launchApp, pwsh, recorder, sleep } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, "0.4.0");
const PORT = 9222;
if (!EXE || !OUT) throw new Error("缺少环境变量 PIERLINK_EXE 或 E2E_OUT");

const LX = {
  host: process.env.PIERLINK_TEST_LINUX_HOST,
  port: process.env.PIERLINK_TEST_LINUX_PORT,
  user: process.env.PIERLINK_TEST_LINUX_USER,
  key: process.env.PIERLINK_TEST_LINUX_KEY,
};
const HAVE_LINUX = !!(LX.host && LX.port && LX.user && LX.key);
const LX_NAME = "pl-linux";
const OTP = { user: process.env.PIERLINK_TEST_LINUX_OTP_USER, pass: process.env.PIERLINK_TEST_LINUX_OTP_PASS, secret: process.env.PIERLINK_TEST_LINUX_OTP_SECRET };
const HAVE_OTP = HAVE_LINUX && !!(OTP.user && OTP.pass && OTP.secret);
const OTP_NAME = "pl-otp";

/** 按 RFC 6238 算当前的 6 位验证码（SHA-1、30 秒一步；secret 是 base32），和手机验证器 App 一样 */
function totp(secret) {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  const bits = [...secret.replace(/=+$/, "").toUpperCase()].map((c) => alphabet.indexOf(c).toString(2).padStart(5, "0")).join("");
  const key = Buffer.from(bits.match(/.{8}/g).map((b) => parseInt(b, 2)));
  const counter = Buffer.alloc(8);
  counter.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 30000)));
  const h = createHmac("sha1", key).update(counter).digest();
  const o = h[h.length - 1] & 0xf;
  return String((h.readUInt32BE(o) & 0x7fffffff) % 1000000).padStart(6, "0");
}

const rec = recorder(OUT);
killApp(EXE);
await sleep(1000);
const { p, u } = await launchApp(EXE, OUT, PORT);

// WSL 里的 sshd 由本脚本启动、测完结束（WSL 可能在两步之间重启过，先建 /run/sshd）
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

/** 以 pl 用户连 WSL sshd 的 ssh.exe 进程个数（系统 OpenSSH 模式每个终端一个，内置模式没有）；参数形如 -F none -p 2222 -l pl */
const sshProcesses = () => Number(pwsh(`@(Get-CimInstance Win32_Process -Filter "Name='ssh.exe'" | Where-Object { $_.CommandLine -match '-p ${LX.port} -l ${LX.user}\\b' }).Count`).split("\n")[0].trim());

/** 填对话框里的文本框（按标签开头匹配） */
async function fill(label, value) {
  const ok = await p.js(`(()=>{const l=[...document.querySelectorAll('.pl-dialog label.pl-field')].find(l=>l.firstChild?.textContent?.trim()===${JSON.stringify(label)} || l.textContent.trim().startsWith(${JSON.stringify(label)})); const i=l?.querySelector('input,textarea'); if(!i) return false; i.focus(); i.select(); return true;})()`);
  if (!ok) throw new Error(`找不到输入框：${label}`);
  await u.type(value);
}

/** 本机找一个空闲端口 */
async function freePort() {
  return new Promise((resolve) => {
    const srv = net.createServer();
    srv.listen(0, "127.0.0.1", () => { const port = srv.address().port; srv.close(() => resolve(port)); });
  });
}

/** 连本机端口读服务器 sshd 的横幅 */
function readBanner(port, timeoutMs = 10000) {
  return new Promise((resolve, reject) => {
    const sock = net.connect(port, "127.0.0.1");
    let buf = Buffer.alloc(0);
    const timer = setTimeout(() => { sock.destroy(); reject(new Error(`读端口 ${port} 超时`)); }, timeoutMs);
    sock.on("data", (d) => { buf = Buffer.concat([buf, d]); if (buf.includes(0x0a)) { clearTimeout(timer); sock.destroy(); resolve(buf.toString()); } });
    sock.on("error", (e) => { clearTimeout(timer); reject(e); });
  });
}

/** 新建一条本地转发（本机 localPort → 服务器上的 remotePort），等它出现在列表里 */
async function newLocalForward(name, localPort, remotePort) {
  await u.clickText("New", ".fwd");
  await u.waitFor(`!!document.querySelector('.pl-dialog')`, "新建端口转发对话框");
  await fill("Name", name);
  await fill("Local port", String(localPort));
  await fill("Port", String(remotePort));
  await u.clickText("Create and start", ".pl-dialog");
}

/** 等转发 name 运行中 */
const waitRunning = (name, ms = 30000) => u.waitFor(`(()=>{const it=[...document.querySelectorAll('.fwd__item')].find(x=>x.querySelector('.fwd__name')?.textContent===${JSON.stringify(name)}); return it && /^Running/.test(it.querySelector('.fwd__state')?.textContent||'');})()`, `转发“${name}”运行中`, ms);

/** 回答内置 SSH 的交互登录：这次运行已经输过密码时只问验证码，否则先问密码 */
async function answerLogin() {
  const kind = await u.waitFor(`(()=>{const f=document.querySelector('.pw-form'); if(!f) return null; return f.querySelector('.pw-form__prompt') ? 'code' : 'password';})()`, "登录框", 30000);
  if (kind === "password") {
    await p.js(`document.querySelector('.pw-form input[type=password]')?.focus()`);
    await u.type(OTP.pass);
    await p.js(`(()=>{const c=document.querySelector('.pw-form input[type=checkbox]'); if(c && c.checked) c.click();})()`);
    await u.clickText("Log in", ".pl-dialog");
    await u.waitFor(`!!document.querySelector('.pw-form__prompt')`, "验证码框", 30000);
  }
  await p.js(`document.querySelector('.pw-form input[type=password]')?.focus()`);
  await u.type(totp(OTP.secret));
  await u.clickText("OK", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pw-form')`, "登录框关闭", 15000);
  return kind;
}

/** 对话框里“连接方式”当前选中的文字 */
const selectedEngine = () => p.js(`document.querySelector('.pl-dialog [role=tablist][aria-label="Connection"] [aria-selected=true]')?.textContent ?? null`);

/** 选中侧栏里的某台服务器，返回行号 */
async function selectServer(name) {
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes(${JSON.stringify(name)})); return i>=0 ? String(i) : null;})()`, `侧栏里的 ${name}`);
  await u.click(".srv-row", Number(row));
  return Number(row);
}

/** 打开服务器的编辑对话框，把连接方式改成 label 对应的一项并保存 */
async function setEngine(name, label) {
  const row = await selectServer(name);
  await u.click(".srv-row", row, "right");
  await u.clickText("Edit", ".pl-menu");
  await u.waitFor(`!!document.querySelector('.pl-dialog [role=tablist][aria-label="Connection"]')`, "编辑服务器对话框里的连接方式");
  await u.clickText(label, ".pl-dialog [role=tablist]");
  await u.waitFor(`(document.querySelector('.pl-dialog [role=tablist][aria-label="Connection"] [aria-selected=true]')?.textContent||'').startsWith(${JSON.stringify(label)})`, `连接方式选中“${label}”`);
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
}

/** 页面表达式：可见窗格里出现 bash 提示符（pl@主机名:~$）的个数 */
const BASH_PANES = `[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none' && /${LX.user}@[^\\s:]+:~\\$/.test(s.querySelector('.xterm-rows')?.innerText||'')).length`;

/** 点一下最后一个可见窗格的终端，让它拿到键盘焦点（终端打开或登录框关掉后不会自己抢焦点） */
async function focusTerminal() {
  const r = await p.js(`(()=>{const v=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); const e=v[v.length-1]?.querySelector('.xterm-screen'); if(!e) return null; const r=e.getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2};})()`);
  if (!r) throw new Error("找不到可见的终端窗格");
  await u.clickXY(r.x, r.y);
}

/** 双击侧栏里的服务器开一个新标签页的终端，等出现 bash 提示符，再执行一条命令确认能用 */
async function openTerminal(name, marker) {
  const slots = await p.js(`document.querySelectorAll('.pane-slot').length`);
  const row = await selectServer(name);
  await u.click(".srv-row", row, "left", 2);
  await u.waitFor(`document.querySelectorAll('.pane-slot').length > ${slots}`, "新开了一个终端", 15000);
  await u.waitFor(`${BASH_PANES} >= 1`, "新终端出现 bash 提示符", 30000);
  await focusTerminal();
  await u.typeLine(`echo ${marker}-$((40+2))`);
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').some(s=>/^${marker}-42$/m.test(s.querySelector('.xterm-rows')?.innerText||''))`, `终端输出 ${marker}-42`, 15000);
}

await rec.step(p, "新建服务器：连接方式默认是内置 SSH；升级说明不会出现在新装或已说明过的列表里", async () => {
  await u.click('[aria-label="Add server"]');
  await u.waitFor(`!!document.querySelector('.pl-dialog [role=tablist][aria-label="Connection"]')`, "添加服务器对话框里的连接方式");
  const selected = await selectedEngine();
  if (!/^Built-in SSH/.test(selected ?? "")) throw new Error(`默认连接方式不是内置 SSH：${selected}`);
  const hint = await p.js(`document.querySelector('.pl-dialog')?.innerText || ''`);
  if (!/n't read ~\/\.ssh\/config/i.test(hint)) throw new Error(`内置 SSH 的说明里没写不读 ~/.ssh/config：${hint.slice(0, 300)}`);
  await u.clickText("Cancel", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
  if (await p.js(`!!document.querySelector('.srv-notice')`)) throw new Error("服务器列表顶部不该再出现内置 SSH 的升级说明");
  return selected;
});

await rec.step(p, "改成系统 OpenSSH：新开的终端由 ssh.exe 连接，照常能用；终端里没有调试信息", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器");
  await setEngine(LX_NAME, "System OpenSSH");
  const before = sshProcesses();
  await openTerminal(LX_NAME, "pl-openssh");
  const after = sshProcesses();
  if (!(after > before)) throw new Error(`系统 OpenSSH 模式下没有新的 ssh.exe 进程：之前 ${before}，之后 ${after}`);
  // Windows 版 OpenSSH 写日志文件时不许别人同时读，所以 Windows 上不让 ssh 写调试日志（错误照常打在终端里）
  const logged = Number(pwsh(`@(Get-CimInstance Win32_Process -Filter "Name='ssh.exe'" | Where-Object { $_.CommandLine -match '-p ${LX.port} -l ${LX.user}\\b' -and $_.CommandLine -match 'LogLevel=DEBUG1' }).Count`).split("\n")[0].trim());
  if (logged !== 0) throw new Error("Windows 上的 ssh.exe 不该带 -o LogLevel=DEBUG1");
  const text = await p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').map(s=>s.querySelector('.xterm-rows')?.innerText||'').join('\\n')`);
  if (/debug\d:/.test(text)) throw new Error(`终端里出现了 ssh 的调试信息：${text.slice(0, 400)}`);
  if (/^(Authenticated to|Transferred: sent|Bytes per second:)/m.test(text)) throw new Error(`终端里出现了 ssh 的 VERBOSE 信息：${text.slice(0, 400)}`);
  return `ssh.exe 进程 ${before} → ${after}`;
});

await rec.step(p, "改回内置 SSH：新开的终端不再启动 ssh.exe，照常能用", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器");
  await setEngine(LX_NAME, "Built-in SSH");
  const before = sshProcesses();
  await openTerminal(LX_NAME, "pl-builtin");
  await sleep(1000);
  const after = sshProcesses();
  if (after > before) throw new Error(`内置模式下又启动了 ssh.exe：之前 ${before}，之后 ${after}`);
  return `ssh.exe 进程 ${before} → ${after}`;
});

await rec.step(p, "从 ssh config 导入的服务器：检查后转换成内置 SSH，照常连接", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器");
  // 在这台测试机当前用户的 ~/.ssh/config 末尾写一个别名（CI 机器用完即弃）
  const sshDir = join(process.env.USERPROFILE, ".ssh");
  mkdirSync(sshDir, { recursive: true });
  const cfg = join(sshDir, "config");
  const before = existsSync(cfg) ? readFileSync(cfg, "utf8") : "";
  writeFileSync(cfg, `${before}\nHost pl-conv\n  HostName ${LX.host}\n  Port ${LX.port}\n  User ${LX.user}\n  IdentityFile ${LX.key}\n  IdentitiesOnly yes\n`);
  // 导入：只勾 pl-conv
  await u.click('[aria-label="Import / export servers"]');
  await u.clickText("Import from ~/.ssh/config", ".pl-menu");
  await u.waitFor(`[...document.querySelectorAll('.imp__alias')].some(a=>a.textContent==='pl-conv')`, "导入列表里有 pl-conv", 30000);
  await p.js(`(()=>{ for (const row of document.querySelectorAll('.imp__row:not(.imp__row--head)')) { const alias=row.querySelector('.imp__alias')?.textContent; const cb=row.querySelector('input[type=checkbox]'); if (cb && !cb.disabled && cb.checked !== (alias==='pl-conv')) cb.click(); } })()`);
  await u.clickText("Import 1 server", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "导入对话框关闭", 20000);
  // 打开它的设置：检查能否转换
  const row = await selectServer("pl-conv");
  await u.click(".srv-row", row, "right");
  await u.clickText("Edit", ".pl-menu");
  await u.waitFor(`!!document.querySelector('.link-convert')`, "设置里的“改用内置 SSH”");
  await u.clickText("Check", ".link-convert");
  const preview = await u.waitFor(`(()=>{const c=document.querySelector('.link-convert'); if(!c) return null; const t=c.innerText; if(/This one can't be switched|Couldn't check/.test(t)) return 'ERR:'+t; return [...c.querySelectorAll('button')].some(b=>b.textContent.trim()==='Switch') ? t : null;})()`, "检查结果", 40000);
  if (preview.startsWith("ERR:")) throw new Error(`不能转换：${preview.slice(4)}`);
  if (!preview.includes(`${LX.host}:${LX.port}`)) throw new Error(`预览里没有 ${LX.host}:${LX.port}：${preview}`);
  await u.clickText("Switch", ".link-convert");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "转换后对话框关闭", 30000);
  // 转换后是应用内添加、用内置 SSH 的服务器：编辑对话框里有“连接方式”，选中内置 SSH
  await u.click(".srv-row", await selectServer("pl-conv"), "right");
  await u.clickText("Edit", ".pl-menu");
  await u.waitFor(`!!document.querySelector('.pl-dialog [role=tablist][aria-label="Connection"]')`, "转换后的编辑对话框");
  const engine = await selectedEngine();
  await u.clickText("Cancel", ".pl-dialog");
  if (!/^Built-in SSH/.test(engine ?? "")) throw new Error(`转换后连接方式不是内置 SSH：${engine}`);
  const procs = sshProcesses();
  await openTerminal("pl-conv", "pl-conv");
  if (sshProcesses() > procs) throw new Error("转换后的服务器还在用 ssh.exe 连接");
  return preview.replace(/\s+/g, " ").slice(0, 200);
});

await rec.step(p, "要验证码的服务器：登录一次（密码 + 验证码），终端、文件面板、资源概览都不再问", async () => {
  if (!HAVE_OTP) throw new Error("没有 WSL 里要验证码的用户（PIERLINK_TEST_LINUX_OTP_*）");
  await u.click('[aria-label="Add server"]');
  await u.waitFor(`!!document.querySelector('.pl-dialog')`, "添加服务器对话框");
  await fill("Host", LX.host);
  await fill("Port", LX.port);
  await fill("User", OTP.user);
  await fill("Display name", OTP_NAME);
  await u.clickText("Password", ".pl-dialog");
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
  const row = await selectServer(OTP_NAME);
  await u.click(".srv-row", row, "left", 2);
  // 同一台服务器（localhost:2222）的指纹在阶段 6 已经信任过；万一再问就信任
  await u.waitFor(`!!document.querySelector('.pw-form') || /Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "密码框或指纹确认", 30000);
  if (!(await p.js(`!!document.querySelector('.pw-form')`))) await u.clickText("Trust and connect", ".pl-dialog");
  // 先问密码（Pierlink 的密码框，所有功能共用）
  const pwText = await u.waitFor(`(()=>{const d=[...document.querySelectorAll('.pl-dialog')].find(x=>x.querySelector('.pw-form')); return d && !d.querySelector('.pw-form__prompt') ? d.innerText : null;})()`, "密码框", 30000);
  if (!new RegExp(`Password for ${OTP_NAME}`).test(pwText)) throw new Error(`密码框文字不对：${pwText}`);
  await p.js(`document.querySelector('.pw-form input[type=password]')?.focus()`);
  await u.type(OTP.pass);
  await p.js(`(()=>{const c=document.querySelector('.pw-form input[type=checkbox]'); if(c && c.checked) c.click();})()`);
  await u.clickText("Log in", ".pl-dialog");
  // 再问验证码：显示服务器的原话
  const prompt = await u.waitFor(`document.querySelector('.pw-form__prompt')?.innerText || null`, "验证码框", 30000);
  if (!/Verification code/i.test(prompt)) throw new Error(`验证码框里的提示不对：${prompt}`);
  await p.js(`document.querySelector('.pw-form input[type=password]')?.focus()`);
  await u.type(totp(OTP.secret));
  await u.clickText("OK", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pw-form')`, "验证码框关闭", 15000);
  const otpPrompt = `[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none' && /${OTP.user}@[^\\s:]+:~\\$/.test(s.querySelector('.xterm-rows')?.innerText||'')).length`;
  await u.waitFor(`${otpPrompt} >= 1`, "验证码用户的 bash 提示符", 30000);
  // 文件面板：在终端里建一个文件，打开这个窗格的文件列表能看到
  await focusTerminal();
  await u.typeLine("touch pl-otp-file && echo pl-otp-$((40+2))");
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').some(s=>/^pl-otp-42$/m.test(s.querySelector('.xterm-rows')?.innerText||''))`, "终端执行了命令", 15000);
  await p.js(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); const b=s[s.length-1]?.querySelector('.pane__btn[aria-label="Files (current directory)"]'); if(b && b.getAttribute('aria-pressed')!=='true') b.click();})()`);
  await u.waitFor(`[...document.querySelectorAll('.files .files__name')].some(n=>n.textContent==='pl-otp-file')`, "文件列表里有 pl-otp-file", 30000);
  // 服务器概况（终端右上角）：后台连接也用同一次登录（不再问密码和验证码）
  const summary = await u.waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').map(s=>s.querySelector('.ov')).find(Boolean); if(!s) return null; const t=s.innerText; if(/Couldn't read|hasn't been entered/.test(t)) return 'ERR:'+t; return s.querySelector('.ov-meters') ? s.querySelector('.ov__summary')?.innerText||null : null;})()`, "概况出数据", 40000);
  if (summary.startsWith("ERR:")) throw new Error(`概况没用上这次登录：${summary}`);
  if (await p.js(`!!document.querySelector('.pw-form')`)) throw new Error("概况或文件面板又弹了密码或验证码框");
  return summary;
});

await rec.step(p, "要验证码的服务器断开后：端口转发提示登录，点一次“登录”（验证码）后恢复", async () => {
  if (!HAVE_OTP) throw new Error("没有 WSL 里要验证码的用户");
  // 经这台服务器的本地转发（连回它自己的 sshd），用刚才那次登录
  if (!(await p.js(`!!document.querySelector('.ws__tool')`))) await u.click('[aria-label^="Tool panel"]');
  await u.waitFor(`!!document.querySelector('.ws__tool')`, "工具面板");
  await u.clickText("Port forwarding", ".ws__tool-head");
  await u.waitFor(`!!document.querySelector('.fwd')`, "端口转发页");
  const port = await freePort();
  await newLocalForward("pl-otp-L", port, Number(LX.port));
  await waitRunning("pl-otp-L", 40000);
  if (!(await readBanner(port)).startsWith("SSH-")) throw new Error("经验证码服务器的本地转发读不到 sshd 横幅");
  // 服务器那头断开这个用户的全部连接
  try { execFileSync("wsl.exe", ["-d", process.env.PIERLINK_TEST_LINUX_WSL, "-u", "root", "--", "pkill", "-u", OTP.user, "-x", "sshd"], { stdio: "ignore" }); } catch { /* 没有进程时 pkill 返回非零 */ }
  // 后台不能自己输验证码：转发提示要登录，而且不自己弹框（终端断开后它的概况随终端一起停）
  await u.waitFor(`(()=>{const it=[...document.querySelectorAll('.fwd__item')].find(x=>x.querySelector('.fwd__name')?.textContent==='pl-otp-L'); return !!it && [...it.querySelectorAll('button')].some(b=>b.textContent.trim()==='Log in');})()`, "转发旁边有“登录”", 90000);
  if (await p.js(`!!document.querySelector('.pw-form')`)) throw new Error("后台功能不该自己弹出登录框");
  // 点转发旁边的“登录”：输一次验证码（这次运行输过的密码还在内存里）
  await u.clickText("Log in", ".fwd");
  const asked = await answerLogin();
  await waitRunning("pl-otp-L", 40000);
  if (!(await readBanner(port)).startsWith("SSH-")) throw new Error("登录后本地转发读不到 sshd 横幅");
  return `登录时问了：${asked === "code" ? "验证码" : "密码和验证码"}`;
});

await rec.step(p, "检查更新：设置页点“检查更新”，30 秒内给出结果，按钮恢复可用", async () => {
  await u.clickText("Settings", "nav");
  await u.waitFor(`[...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='Check for updates')`, "检查更新按钮");
  await u.clickText("Check for updates");
  // CI 里可能已是最新、发现新版本（会弹出底部的更新提示）或者检查失败（发布页没有更新信息时）；三种都算给出了结果
  const result = await u.waitFor(`(()=>{const t=document.body.innerText; const m=t.match(/You're on the latest version|Couldn't check: [^\\n]+|Pierlink [0-9.]+ is available/); const idle=[...document.querySelectorAll('button')].some(b=>b.textContent.trim()==='Check for updates' && !b.disabled); return m && idle ? m[0] : null;})()`, "检查更新的结果", 30000);
  if (/available/.test(result)) await u.clickText("Later").catch(() => {});
  await u.clickText("Workspace", "nav");
  return result;
});

p.close();
killApp(EXE);
if (wslSshd) {
  try { execFileSync("wsl.exe", ["-d", process.env.PIERLINK_TEST_LINUX_WSL, "-u", "root", "--", "pkill", "-x", "sshd"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
  wslSshd.kill();
}
process.exitCode = rec.finish() ? 1 : 0;
