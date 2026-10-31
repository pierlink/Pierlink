// 0.9.7 粘贴图片和代理的 Windows 端到端测试。
// 粘贴图片：剪贴板里只有图片时按 Ctrl+V，服务器终端把图片传到服务器的 /tmp（只有自己能读）、本机终端存到本机数据目录，
// 再把路径粘贴进去；剪贴板里是位图（转成 PNG）和现成的 PNG 两种都要能用。
// 代理：设置页选自定义 HTTP / SOCKS5（本脚本在本机起的测试代理），“测试代理”可用；内置 SSH 和系统 OpenSSH
// 连服务器都经过代理（系统 OpenSSH 由 Pierlink.exe --proxy-connect 当 ProxyCommand）；本机终端有代理环境变量；
// 改回“不使用”后直连。服务器用 WSL 的地址（不是 localhost：本机地址永远直连）。
// 在 stage6.mjs 之后运行：那里已经添加了 WSL 里的 Linux 服务器 pl-linux（内置 SSH、指定私钥、指纹已信任）。
//
// 环境变量：PIERLINK_EXE、E2E_OUT（必填）；PIERLINK_TEST_LINUX_*（同 stage6.mjs，没有时服务器相关步骤失败）
// 每一步截图；某一步失败不中断后面的步骤。全部通过时退出码为 0。
import net from "node:net";
import { basename, dirname, join } from "node:path";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { cpSync, mkdirSync, openSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { killApp, launchApp, pwsh, recorder, sleep } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, "0.9.7-paste-proxy");
const PORT = 9222;
if (!EXE || !OUT) throw new Error("缺少环境变量 PIERLINK_EXE 或 E2E_OUT");
mkdirSync(OUT, { recursive: true });
const LX = {
  host: process.env.PIERLINK_TEST_LINUX_HOST,
  port: process.env.PIERLINK_TEST_LINUX_PORT,
  user: process.env.PIERLINK_TEST_LINUX_USER,
  key: process.env.PIERLINK_TEST_LINUX_KEY,
  wsl: process.env.PIERLINK_TEST_LINUX_WSL,
};
const HAVE_LINUX = !!(LX.host && LX.port && LX.user && LX.key && LX.wsl);
const LX_NAME = "pl-linux";
/** 经代理连接用的服务器：同一个 WSL sshd，地址换成 WSL 的 IP */
const PX_NAME = "pl-proxy";
/** 复制一份 Pierlink 到带空格和中文的目录（很多人的 Windows 用户名带空格或中文，按用户安装时路径里就有） */
const SPACED_DIR = join(process.env.LOCALAPPDATA ?? "", "Pier link 测试");
/** 本机终端存图片的目录（数据目录下的 paste） */
const LOCAL_PASTE_DIR = join(process.env.LOCALAPPDATA ?? "", "com.pierlink.desktop", "paste");
/** 粘贴进去的服务器路径 */
const REMOTE_PATH = /\/tmp\/pierlink-paste-\d{8}-\d{6}-[0-9a-f]{10}\.png/;
/** 测试图片的宽、高；PNG 开头 24 字节（签名、IHDR 长度和类型、宽、高）的十六进制 */
const IMG_W = 64;
const IMG_H = 48;
const PNG_HEAD = `89504e470d0a1a0a0000000d49484452${IMG_W.toString(16).padStart(8, "0")}${IMG_H.toString(16).padStart(8, "0")}`;

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

// ---------- 测试代理：HTTP CONNECT 和 SOCKS5（不要密码），记下每次连接的目标再转发过去 ----------
/** 代理收到的连接：{ kind: "http" | "socks5", host, port } */
const targets = [];
/** 连上目标后把两边接起来；连不上时关掉客户端 */
function bridge(client, host, port, onUp, early) {
  const up = net.connect(port, host, () => {
    onUp();
    if (early.length) up.write(early);
    up.pipe(client);
    client.pipe(up);
  });
  up.on("error", () => client.destroy());
  client.on("error", () => up.destroy());
  client.on("close", () => up.destroy());
}
const httpProxy = net.createServer((client) => {
  let buf = Buffer.alloc(0);
  const onData = (d) => {
    buf = Buffer.concat([buf, d]);
    const end = buf.indexOf("\r\n\r\n");
    if (end < 0) return;
    client.off("data", onData);
    const line = buf.subarray(0, buf.indexOf("\r\n")).toString();
    const m = /^CONNECT \[?([^\]\s]+?)\]?:(\d+) HTTP\/1\.[01]$/.exec(line);
    if (!m) { client.end("HTTP/1.1 400 Bad Request\r\n\r\n"); return; }
    targets.push({ kind: "http", host: m[1], port: Number(m[2]) });
    bridge(client, m[1], Number(m[2]), () => client.write("HTTP/1.1 200 Connection established\r\n\r\n"), buf.subarray(end + 4));
  };
  client.on("data", onData);
  client.on("error", () => {});
});
const socksProxy = net.createServer((client) => {
  let buf = Buffer.alloc(0);
  let greeted = false;
  const onData = (d) => {
    buf = Buffer.concat([buf, d]);
    if (!greeted) {
      if (buf.length < 2 || buf.length < 2 + buf[1]) return;
      const methods = [...buf.subarray(2, 2 + buf[1])];
      buf = buf.subarray(2 + buf[1]);
      if (!methods.includes(0)) { client.end(Buffer.from([5, 0xff])); return; }
      client.write(Buffer.from([5, 0]));
      greeted = true;
    }
    if (buf.length < 5) return;
    let host;
    let at;
    if (buf[3] === 1) { if (buf.length < 10) return; host = [...buf.subarray(4, 8)].join("."); at = 8; }
    else if (buf[3] === 3) { if (buf.length < 7 + buf[4]) return; host = buf.subarray(5, 5 + buf[4]).toString(); at = 5 + buf[4]; }
    else { client.end(Buffer.from([5, 8, 0, 1, 0, 0, 0, 0, 0, 0])); return; }
    const port = buf.readUInt16BE(at);
    client.off("data", onData);
    targets.push({ kind: "socks5", host, port });
    bridge(client, host, port, () => client.write(Buffer.from([5, 0, 0, 1, 0, 0, 0, 0, 0, 0])), buf.subarray(at + 2));
  };
  client.on("data", onData);
  client.on("error", () => {});
});
const listen = (srv) => new Promise((r) => srv.listen(0, "127.0.0.1", () => r(srv.address().port)));
const HTTP_PORT = await listen(httpProxy);
const SOCKS_PORT = await listen(socksProxy);
console.log(`测试代理: HTTP=127.0.0.1:${HTTP_PORT}, SOCKS5=127.0.0.1:${SOCKS_PORT}`);
/** 代理收到过的、到 WSL sshd 的连接次数 */
const viaProxy = (kind, host) => targets.filter((t) => t.kind === kind && t.host === host && t.port === Number(LX.port)).length;

/** WSL 的 IP（第一个不是 127 开头的 IPv4） */
function wslIp() {
  const out = execFileSync("wsl.exe", ["-d", LX.wsl, "--", "hostname", "-I"], { encoding: "utf8" });
  const ip = out.split(/\s+/).find((x) => /^\d+\.\d+\.\d+\.\d+$/.test(x) && !x.startsWith("127."));
  if (!ip) throw new Error(`取不到 WSL 的 IP：${out}`);
  return ip;
}

// ---------- 页面操作 ----------
/** 当前可见窗格里终端的文字 */
const TERM = `([...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]?.querySelector('.xterm-rows')?.innerText||'')`;
/** 可见窗格里出现 bash 提示符（pl@主机名:~$） */
const BASH_PROMPT = `/${LX.user}@[^\\s:]+:~\\$/.test(${TERM})`;
/** PowerShell 提示符 */
const PS_PROMPT = `/PS [^>]*>/.test(${TERM})`;

async function clickVisible(selector) {
  const r = await p.js(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]; const e=s?.querySelector(${JSON.stringify(selector)}); if(!e) return null; const b=e.getBoundingClientRect(); return {x:b.x+b.width/2,y:b.y+b.height/2};})()`);
  if (!r) throw new Error(`可见窗格里找不到：${selector}`);
  await u.clickXY(r.x, r.y);
}

/** 服务器列表里某台服务器的行号 */
async function serverRow(name) {
  return Number(await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes(${JSON.stringify(name)})); return i>=0 ? String(i) : null;})()`, `服务器列表里有 ${name}`));
}

/** 双击服务器开一个新终端标签，第一次连接时信任指纹，等到 bash 提示符，执行一条命令确认能用 */
async function openServer(name, marker) {
  await u.clickText("Workspace", "header nav");
  const slots = await p.js(`document.querySelectorAll('.pane-slot').length`);
  await u.click(".srv-row", await serverRow(name), "left", 2);
  await u.waitFor(`document.querySelectorAll('.pane-slot').length > ${slots} || /Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "新开了终端或出现指纹确认", 30000);
  if (await p.js(`/Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'')`)) {
    await u.clickText("Trust and connect", ".pl-dialog");
    await u.waitFor(`!document.querySelector('.pl-dialog')`, "指纹确认对话框关闭", 30000);
  }
  await u.waitFor(BASH_PROMPT, "服务器终端出现 bash 提示符", 30000);
  await clickVisible(".xterm-screen");
  await sleep(300);
  await u.typeLine(`echo ${marker}-$((40+2))`);
  await u.waitFor(`/^${marker}-42$/m.test(${TERM})`, `终端输出 ${marker}-42`, 15000);
}

/** 把服务器的连接方式改成 label 对应的一项（同 stage7.mjs） */
async function setEngine(name, label) {
  await u.clickText("Workspace", "header nav");
  await u.click(".srv-row", await serverRow(name), "right");
  await u.clickText("Edit", ".pl-menu");
  await u.waitFor(`!!document.querySelector('.pl-dialog [role=tablist][aria-label="Connection"]')`, "编辑服务器对话框里的连接方式");
  await u.clickText(label, ".pl-dialog [role=tablist]");
  await u.waitFor(`(document.querySelector('.pl-dialog [role=tablist][aria-label="Connection"] [aria-selected=true]')?.textContent||'').startsWith(${JSON.stringify(label)})`, `连接方式选中“${label}”`);
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
}

/** 填对话框里的文本框（按标签开头匹配） */
async function fill(label, value) {
  const ok = await p.js(`(()=>{const l=[...document.querySelectorAll('.pl-dialog label.pl-field')].find(l=>l.firstChild?.textContent?.trim()===${JSON.stringify(label)} || l.textContent.trim().startsWith(${JSON.stringify(label)})); const i=l?.querySelector('input,textarea'); if(!i) return false; i.focus(); i.select(); return true;})()`);
  if (!ok) throw new Error(`找不到输入框：${label}`);
  await u.type(value);
}

/** 按 Ctrl+V */
const ctrlV = () => u.key("v", "KeyV", 86, 2);

/**
 * 往 Windows 剪贴板放一张 64×48 的红色图片。
 * @param format "bitmap"：只有位图（Pierlink 转成 PNG）；"png"：只有“PNG”格式（浏览器、截图工具常这样放）
 */
function setClipboardImage(format) {
  const script = [
    "Add-Type -AssemblyName System.Windows.Forms, System.Drawing",
    `$bmp = New-Object System.Drawing.Bitmap ${IMG_W}, ${IMG_H}`,
    "$g = [System.Drawing.Graphics]::FromImage($bmp); $g.Clear([System.Drawing.Color]::FromArgb(255, 200, 30, 30)); $g.Dispose()",
    format === "png"
      ? "$ms = New-Object System.IO.MemoryStream; $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); $d = New-Object System.Windows.Forms.DataObject; $d.SetData('PNG', $false, $ms); [System.Windows.Forms.Clipboard]::SetDataObject($d, $true)"
      : "[System.Windows.Forms.Clipboard]::SetImage($bmp)",
    "'clipboard-ok'",
  ].join("\n");
  // 剪贴板要在 STA 线程里操作：用 Windows PowerShell 加 -STA
  const r = spawnSync("powershell.exe", ["-STA", "-NoProfile", "-NonInteractive", "-Command", script], { encoding: "utf8" });
  if (!/clipboard-ok/.test(r.stdout || "")) throw new Error(`往剪贴板放图片失败：${r.stdout}${r.stderr}`);
}

/** 在 WSL 里看服务器上的图片：权限、大小、属主和 PNG 开头 24 字节 */
function remoteImage(path) {
  const out = execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "root", "--", "sh", "-c", `stat -c '%a %s %U' '${path}' && od -An -tx1 -N24 '${path}' | tr -d ' \\n'`], { encoding: "utf8" });
  const [stat, head] = out.trim().split("\n");
  return { stat: stat.trim(), head: (head || "").trim() };
}

/** 服务器终端里粘贴一张图片：输入 f=、按 Ctrl+V、回车，返回粘贴进去的路径 */
async function pasteIntoServer(format, marker) {
  setClipboardImage(format);
  await openServer(LX_NAME, marker);
  await u.type("f=");
  await ctrlV();
  const path = await u.waitFor(`(()=>{const m=${REMOTE_PATH}.exec(${TERM}.replace(/\\n/g,'')); return m ? m[0] : null;})()`, "终端里出现粘贴的图片路径", 40000);
  await u.enter();
  await u.typeLine(`echo PLF=$f`);
  await u.waitFor(`/^PLF=\\/tmp\\/pierlink-paste-/m.test(${TERM})`, "路径进了 shell 变量（是粘贴进命令行，不是被执行）", 10000);
  const img = remoteImage(path);
  if (!new RegExp(`^600 \\d+ ${LX.user}$`).test(img.stat)) throw new Error(`服务器上的图片权限、属主不对（应只有自己能读）：${img.stat}`);
  if (img.head !== PNG_HEAD) throw new Error(`服务器上的不是 ${IMG_W}×${IMG_H} 的 PNG：开头 ${img.head}`);
  return `${path}（${img.stat}）`;
}

await rec.step(conn, "服务器终端粘贴图片（剪贴板里是位图）：路径粘贴进命令行，图片在服务器 /tmp、只有自己能读、是 PNG", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器的环境变量");
  return await pasteIntoServer("bitmap", "pl-img1");
});

await rec.step(conn, "服务器终端粘贴图片（剪贴板里是现成的 PNG）", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器的环境变量");
  return await pasteIntoServer("png", "pl-img2");
});

await rec.step(conn, "本机 PowerShell 粘贴图片：路径是本机数据目录里的 PNG", async () => {
  setClipboardImage("bitmap");
  await u.clickText("Workspace", "header nav");
  await u.click('.tabbar button[aria-label="New terminal tab"]');
  await u.waitFor(PS_PROMPT, "PowerShell 提示符", 30000);
  await clickVisible(".xterm-screen");
  await sleep(300);
  await u.type("$f = '");
  await ctrlV();
  const path = await u.waitFor(`(()=>{const m=/[A-Z]:\\\\[^']*pierlink-paste-\\d{8}-\\d{6}-[0-9a-f]{10}\\.png/.exec(${TERM}.replace(/\\n/g,'')); return m ? m[0] : null;})()`, "终端里出现粘贴的本机路径", 20000);
  await u.type("'");
  await u.enter();
  await u.typeLine(`"PLLEN=$((Get-Item $f).Length)"`);
  await u.waitFor(`/^PLLEN=\\d+$/m.test(${TERM})`, "PowerShell 能读到这个文件", 10000);
  const newest = readdirSync(LOCAL_PASTE_DIR).filter((n) => n.startsWith("pierlink-paste-")).map((n) => join(LOCAL_PASTE_DIR, n)).sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
  if (!newest) throw new Error(`本机目录里没有图片：${LOCAL_PASTE_DIR}`);
  const head = readFileSync(newest).subarray(0, 24).toString("hex");
  if (head !== PNG_HEAD) throw new Error(`本机图片不是 ${IMG_W}×${IMG_H} 的 PNG：开头 ${head}`);
  return path;
});

// ---------- 代理 ----------
/** 打开设置页的“网络”一节 */
async function openNetwork() {
  await u.clickText("Settings", "header nav");
  await u.clickText("Network", ".settings__nav");
  await u.waitFor(`!!document.querySelector('[role=tablist][aria-label="Proxy"]')`, "设置页的代理");
}

/** React 受控的输入框：直接设值并触发 input 事件 */
async function setInput(selector, value) {
  const ok = await p.js(`(()=>{const e=document.querySelector(${JSON.stringify(selector)}); if(!e) return false; const proto=e.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype; Object.getOwnPropertyDescriptor(proto,'value').set.call(e, ${JSON.stringify(value)}); e.dispatchEvent(new Event('input',{bubbles:true})); return true;})()`);
  if (!ok) throw new Error(`找不到输入框：${selector}`);
}

/** 设置代理并保存：mode 为 Off / System / Custom；Custom 时 kind 为 HTTP / SOCKS5，名单清空 */
async function saveProxy(mode, kind, port) {
  await openNetwork();
  await u.clickText(mode, '[role=tablist][aria-label="Proxy"]');
  if (mode === "Custom") {
    await u.waitFor(`!!document.querySelector('[role=tablist][aria-label="Protocol"]')`, "自定义代理的协议和地址");
    await u.clickText(kind, '[role=tablist][aria-label="Protocol"]');
    await setInput('input[aria-label="Host"]', "127.0.0.1");
    await setInput('input[aria-label="Port"]', String(port));
    // 名单清空：WSL 的地址在默认名单的 172.16.0.0/12 里，不清空就直连了
    await setInput(".proxy-bypass textarea", "");
  }
  if (await p.js(`!document.querySelector('.proxy-actions button.pl-btn--primary')?.disabled`)) {
    await u.clickText("Save", ".proxy-actions");
    await u.waitFor(`/^Saved$/.test(document.querySelector('.proxy-status')?.textContent||'')`, "显示已保存", 10000);
  }
}

let PX_IP = null;

await rec.step(conn, "设置自定义 HTTP 代理并保存；“测试代理”经代理连 github.com 可用", async () => {
  await saveProxy("Custom", "HTTP", HTTP_PORT);
  await u.clickText("Test proxy", ".proxy-actions");
  const text = await u.waitFor(`(()=>{const t=document.querySelector('.proxy-status')?.textContent||''; return /^Works, \\d+ ms$/.test(t) || /proxy|Can't|refused|respond/i.test(t) ? t : null;})()`, "测试结果", 30000);
  if (!/^Works/.test(text)) throw new Error(`测试代理失败：${text}`);
  if (!targets.some((t) => t.kind === "http" && t.host === "github.com" && t.port === 443)) throw new Error(`测试代理没有经过本机代理：${JSON.stringify(targets)}`);
  return text;
});

await rec.step(conn, "经 HTTP 代理连服务器（内置 SSH）：代理收到到 WSL 地址的连接，终端能用", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL Linux 服务器的环境变量");
  PX_IP = wslIp();
  await u.clickText("Workspace", "header nav");
  await u.click('[aria-label="Add server"]');
  await u.waitFor(`!!document.querySelector('.pl-dialog')`, "添加服务器对话框");
  await fill("Host", PX_IP);
  await fill("Port", LX.port);
  await fill("User", LX.user);
  await fill("Display name", PX_NAME);
  await u.clickText("Specific key", ".pl-dialog");
  await fill("Private key file", LX.key);
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
  const before = viaProxy("http", PX_IP);
  await openServer(PX_NAME, "pl-px1");
  const after = viaProxy("http", PX_IP);
  if (!(after > before)) throw new Error(`连接没有经过 HTTP 代理：${JSON.stringify(targets)}`);
  return `WSL 地址 ${PX_IP}，经代理 ${before} → ${after}`;
});

await rec.step(conn, "系统 OpenSSH 经 HTTP 代理：ssh.exe 用 Pierlink.exe --proxy-connect 当 ProxyCommand，终端能用", async () => {
  if (!PX_IP) throw new Error("上一步没有添加经代理的服务器");
  await setEngine(PX_NAME, "System OpenSSH");
  const before = viaProxy("http", PX_IP);
  await openServer(PX_NAME, "pl-px2");
  const helpers = Number(pwsh(`@(Get-CimInstance Win32_Process -Filter "Name='Pierlink.exe'" | Where-Object { $_.CommandLine -match '--proxy-connect' }).Count`).split("\n")[0].trim());
  if (!(helpers >= 1)) throw new Error("没有找到 Pierlink.exe --proxy-connect 进程");
  const after = viaProxy("http", PX_IP);
  if (!(after > before)) throw new Error(`系统 OpenSSH 的连接没有经过代理：${JSON.stringify(targets)}`);
  return `代理入口进程 ${helpers} 个，经代理 ${before} → ${after}`;
});

await rec.step(conn, "Pierlink 在带空格和中文的目录里运行：系统 OpenSSH 的 ProxyCommand 照常能用", async () => {
  if (!PX_IP) throw new Error("前面没有添加经代理的服务器");
  p.close();
  killApp(EXE);
  await sleep(1500);
  rmSync(SPACED_DIR, { recursive: true, force: true });
  cpSync(dirname(EXE), SPACED_DIR, { recursive: true });
  ({ p, u } = await launchApp(join(SPACED_DIR, basename(EXE)), OUT, PORT));
  const before = viaProxy("http", PX_IP);
  await openServer(PX_NAME, "pl-px2b");
  const helpers = Number(pwsh(`@(Get-CimInstance Win32_Process -Filter "Name='Pierlink.exe'" | Where-Object { $_.CommandLine -match '--proxy-connect' -and $_.CommandLine -match 'Pier link' }).Count`).split("\n")[0].trim());
  if (!(helpers >= 1)) throw new Error("没有找到从带空格目录启动的 Pierlink.exe --proxy-connect 进程");
  const after = viaProxy("http", PX_IP);
  if (!(after > before)) throw new Error(`从带空格目录运行时系统 OpenSSH 没有经过代理：${JSON.stringify(targets)}`);
  return `${SPACED_DIR}，经代理 ${before} → ${after}`;
});

await rec.step(conn, "本机 PowerShell 有 HTTP 代理的环境变量", async () => {
  await u.clickText("Workspace", "header nav");
  await u.click('.tabbar button[aria-label="New terminal tab"]');
  await u.waitFor(PS_PROMPT, "PowerShell 提示符", 30000);
  await clickVisible(".xterm-screen");
  await sleep(300);
  await u.typeLine(`"PLPX=$env:HTTPS_PROXY|$env:ALL_PROXY|$env:NO_PROXY"`);
  const want = `PLPX=http://127.0.0.1:${HTTP_PORT}|http://127.0.0.1:${HTTP_PORT}|localhost,127.0.0.1,::1`;
  await u.waitFor(`${TERM}.split('\\n').some(l=>l.trim()===${JSON.stringify(want)})`, `终端输出 ${want}`, 10000);
  return want;
});

await rec.step(conn, "改成 SOCKS5 代理：系统 OpenSSH 和内置 SSH 新开的终端都经 SOCKS5；本机终端只设 ALL_PROXY", async () => {
  if (!PX_IP) throw new Error("前面没有添加经代理的服务器");
  await saveProxy("Custom", "SOCKS5", SOCKS_PORT);
  const s0 = viaProxy("socks5", PX_IP);
  await openServer(PX_NAME, "pl-px3");
  const s1 = viaProxy("socks5", PX_IP);
  if (!(s1 > s0)) throw new Error(`系统 OpenSSH 没有经过 SOCKS5：${JSON.stringify(targets)}`);
  await setEngine(PX_NAME, "Built-in SSH");
  await openServer(PX_NAME, "pl-px4");
  const s2 = viaProxy("socks5", PX_IP);
  if (!(s2 > s1)) throw new Error(`内置 SSH 没有经过 SOCKS5：${JSON.stringify(targets)}`);
  await u.click('.tabbar button[aria-label="New terminal tab"]');
  await u.waitFor(PS_PROMPT, "PowerShell 提示符", 30000);
  await clickVisible(".xterm-screen");
  await sleep(300);
  await u.typeLine(`"PLPX=$env:HTTPS_PROXY|$env:ALL_PROXY"`);
  const want = `PLPX=|socks5h://127.0.0.1:${SOCKS_PORT}`;
  await u.waitFor(`${TERM}.split('\\n').some(l=>l.trim()===${JSON.stringify(want)})`, `终端输出 ${want}`, 10000);
  return `SOCKS5 连接 ${s0} → ${s2}；${want}`;
});

await rec.step(conn, "改回“不使用”：之后的新连接直连，本机终端没有代理变量", async () => {
  if (!PX_IP) throw new Error("前面没有添加经代理的服务器");
  await saveProxy("Off");
  const before = targets.length;
  await openServer(PX_NAME, "pl-px5");
  if (targets.length !== before) throw new Error(`关掉代理后还在经过代理：${JSON.stringify(targets.slice(before))}`);
  await u.click('.tabbar button[aria-label="New terminal tab"]');
  await u.waitFor(PS_PROMPT, "PowerShell 提示符", 30000);
  await clickVisible(".xterm-screen");
  await sleep(300);
  await u.typeLine(`"PLPX=[$env:ALL_PROXY]"`);
  await u.waitFor(`${TERM}.split('\\n').some(l=>l.trim()==='PLPX=[]')`, "本机终端没有 ALL_PROXY", 10000);
  return "直连";
});

p.close();
killApp(EXE);
httpProxy.close();
// 带空格目录里的副本：程序退出后删掉
await sleep(1500);
try { rmSync(SPACED_DIR, { recursive: true, force: true }); } catch { /* 文件还被占着时留给 CI 清理 */ }
socksProxy.close();
if (wslSshd) {
  try { execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "root", "--", "pkill", "-x", "sshd"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
  wslSshd.kill();
}
process.exitCode = rec.finish() ? 1 : 0;
