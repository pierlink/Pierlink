// 0.8.0 的 Windows 端到端测试：服务器桌面（VNC）。
// 在 WSL 的 Linux 服务器 pl-linux 上起两个临时 TigerVNC 桌面（wsl-vnc.sh）：:5 要密码（蓝色）、:6 只开套接字不要密码（绿色）。
// 在 Pierlink 里像用户一样打开桌面、选桌面、输密码，按画面颜色确认连的是哪个；关掉窗格后服务器上的桌面照常运行。
// 在 stage8.mjs 之后运行：pl-linux 已添加且信任了指纹。
//
// 环境变量：PIERLINK_EXE、E2E_OUT（必填）；PIERLINK_TEST_LINUX_*（同 stage6.mjs，没有时跳过）
import net from "node:net";
import { join } from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, openSync } from "node:fs";
import { killApp, launchApp, recorder, sleep } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, "0.8.0");
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
const VNC_PASSWORD = "pl-e2e-vnc";

const rec = recorder(OUT);
killApp(EXE);
await sleep(1000);
const { p, u } = await launchApp(EXE, OUT, PORT);

let wslSshd = null;
let vnc = null;
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

const near = (rgb, want) => !!rgb && rgb.split(",").map(Number).every((v, i) => Math.abs(v - want[i]) <= 24);
const status = () => p.js(`document.querySelector('.desk__status')?.innerText || ''`);

/** 侧栏里右键 pl-linux →“Open desktop” */
async function openDesktop() {
  // 前面阶段可能开着右侧工具面板：它会盖住桌面卡片右边的按钮，先收起来
  await p.js(`document.querySelector('[aria-label="Hide panel"]')?.click()`);
  await sleep(300);
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes(${JSON.stringify(LX_NAME)})); return i>=0 ? String(i) : null;})()`, `侧栏里的 ${LX_NAME}`);
  await u.click(".srv-row", Number(row), "right");
  await u.clickText("Open desktop", ".pl-menu");
}

await rec.step(p, "服务器桌面：WSL 里起两个桌面（:5 要密码、:6 只开套接字）", async () => {
  if (!HAVE_LINUX) return "跳过：没有 WSL 里的 Linux 服务器";
  const log = openSync(join(OUT, "wsl-vnc.log"), "a");
  vnc = spawn("wsl.exe", ["-d", LX.wsl, "-u", "pl", "--", "sh", "tests/e2e-windows/wsl-vnc.sh", VNC_PASSWORD], { stdio: ["ignore", "pipe", log] });
  let out = "";
  vnc.stdout.on("data", (d) => { out += d.toString(); });
  const deadline = Date.now() + 30000;
  while (!out.includes("ready")) {
    if (vnc.exitCode !== null) throw new Error(`wsl-vnc.sh 退出了：${out}`);
    if (Date.now() > deadline) throw new Error(`30 秒内桌面没起来：${out}`);
    await sleep(300);
  }
  return "两个桌面已就绪";
});

await rec.step(p, "打开桌面：列出两个桌面；连只开套接字的 :6（不要密码），画面是绿色", async () => {
  if (!vnc) return "跳过";
  await openDesktop();
  await u.waitFor(`document.querySelectorAll('.desk__row').length >= 2`, "桌面列表里有两个桌面", 30000);
  const rows = await p.js(`[...document.querySelectorAll('.desk__row')].map(r=>r.innerText.replace(/\\s+/g,' ')).join(' | ')`);
  const i6 = await p.js(`[...document.querySelectorAll('.desk__row')].findIndex(r=>r.querySelector('.mono')?.textContent===':6')`);
  if (i6 < 0) throw new Error(`列表里没有 :6：${rows}`);
  await u.click(".desk__row button", i6);
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected')`, "已连接", 30000);
  const px = await u.waitFor(centerPixelExpr(), "画面出来", 15000);
  if (!near(px, [0x30, 0xc0, 0x60])) throw new Error(`画面颜色不对：${px}（列表：${rows}；状态：${await status()}）`);
  return `${await status()}；列表：${rows}`;
});

/** 画布中间一个像素的颜色（noVNC 的画布就是远程桌面；还没画出来时为 null） */
function centerPixelExpr() {
  return `(()=>{const c=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').map(s=>s.querySelector('.desk__screen canvas')).find(Boolean); if(!c||!c.width) return null; const d=c.getContext('2d').getImageData(Math.floor(c.width/2), Math.floor(c.height/2), 1, 1).data; return (d[0]+d[1]+d[2])>0 ? [d[0],d[1],d[2]].join(',') : null;})()`;
}

await rec.step(p, "换成要密码的 :5：输密码后连上、画面是蓝色；断开再连不用再输（密码已记住）", async () => {
  if (!vnc) return "跳过";
  await u.click('[aria-label="Disconnect"]');
  await u.clickText("Choose another desktop", ".desk__card");
  await u.waitFor(`document.querySelectorAll('.desk__row').length >= 2`, "桌面列表", 30000);
  const i5 = await p.js(`[...document.querySelectorAll('.desk__row')].findIndex(r=>r.querySelector('.mono')?.textContent===':5')`);
  if (i5 < 0) throw new Error("列表里没有 :5");
  await u.click(".desk__row button", i5);
  await u.waitFor(`!!document.querySelector('.desk__card input[type=password]')`, "密码框", 30000);
  await u.click(".desk__card input[type=password]");
  await u.type(VNC_PASSWORD);
  await u.clickText("Connect", ".desk__card");
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected')`, "已连接", 30000);
  const px = await u.waitFor(centerPixelExpr(), "画面出来", 15000);
  if (!near(px, [0x40, 0x80, 0xff])) throw new Error(`画面颜色不对：${px}`);
  await u.click('[aria-label="Disconnect"]');
  await u.clickText("Reconnect", ".desk__card");
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected')`, "不输密码直接连上", 30000);
  if (await p.js(`!!document.querySelector('.desk__card input[type=password]')`)) throw new Error("记住的密码没有用上");
  return await status();
});

/** :5 桌面的 X 剪贴板（CLIPBOARD）里的文字；读不到时为空 */
const remoteClipboard = () => {
  try {
    return execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "pl", "--", "sh", "-c", 'DISPLAY=:5 XAUTHORITY="$HOME/.vnc-e2e/x5.auth" timeout 5 xclip -o -selection clipboard 2>/dev/null'], { encoding: "utf8" });
  } catch {
    return "";
  }
};

await rec.step(p, "在桌面画面里按 Ctrl+V：远程剪贴板收到 Windows 剪贴板的文字；断开重连后同样的文字也能再发过去", async () => {
  if (!vnc) return "跳过";
  execFileSync("powershell.exe", ["-NoProfile", "-Command", "Set-Clipboard -Value 'PLDESKPASTE'"]);
  const paste = async () => {
    await u.click(".desk__screen canvas");
    await sleep(300);
    await u.key("v", "KeyV", 86, 2);
  };
  const waitRemote = async (what) => {
    const deadline = Date.now() + 10000;
    let got = "";
    while (Date.now() < deadline) {
      got = remoteClipboard();
      if (got === "PLDESKPASTE") return;
      await sleep(500);
    }
    throw new Error(`${what}：远程剪贴板是 ${JSON.stringify(got.slice(0, 80))}，应该是 "PLDESKPASTE"`);
  };
  await paste();
  await waitRemote("按 Ctrl+V 后");
  await u.click('[aria-label="Disconnect"]');
  await u.clickText("Reconnect", ".desk__card");
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected')`, "重新连上", 30000);
  await sleep(1000);
  await paste();
  await waitRemote("重连后再按 Ctrl+V");
  return "两次远程剪贴板都收到了";
});

await rec.step(p, "全屏：和终端分屏时也盖住整个窗口（旁边的窗格、分隔条、侧栏都在下面）；再点一次退出", async () => {
  if (!vnc) return "跳过";
  // 先在桌面旁边开一个本机终端
  await u.click('.desk__bar .pane__btn--split');
  const item = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('[role=menuitem]')].findIndex(x=>/PowerShell|Command Prompt/.test(x.textContent)); return i>=0 ? String(i) : null;})()`, "分屏菜单里的本机终端", 5000);
  await u.click("[role=menuitem]", Number(item));
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').length === 2`, "桌面旁边多了一个终端", 15000);
  await sleep(1500);
  await u.click('.desk__bar button[aria-label="Full screen"]');
  await u.waitFor(`document.fullscreenElement === document.documentElement && !!document.querySelector('.desk--full')`, "进入全屏", 8000);
  // 窗口里各处点下去都应该是全屏的桌面
  const missed = await p.js(`(()=>{const pts=[]; for (const s of [...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')) { const r=s.getBoundingClientRect(); pts.push(['窗格', r.x+r.width/2, r.y+r.height/2]); } pts.push(['左上', 20, 20], ['右下', innerWidth-20, innerHeight-20], ['左边', 60, innerHeight/2]); return pts.filter(([,x,y])=>!document.elementFromPoint(x,y)?.closest('.desk--full')).map(([n])=>n);})()`);
  if (missed.length) throw new Error(`全屏后这些地方没被桌面盖住：${missed.join("、")}`);
  await u.click('.desk__bar button[aria-label="Full screen"]');
  await u.waitFor(`!document.fullscreenElement && !document.querySelector('.desk--full')`, "退出全屏", 8000);
  // 关掉旁边的终端
  const close = await p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').findIndex(s=>!s.querySelector('.desk'))`);
  await p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[${close}]?.querySelector('.pane__btn--close')?.click()`);
  // 终端还在启动时会问一次“仍在运行”，直接确认关闭
  await sleep(800);
  await p.js(`document.querySelector('.pl-dialog .pl-btn--danger, .pl-dialog .pl-btn--primary')?.click()`);
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').length === 1`, "关掉旁边的终端", 10000);
  return "分屏时全屏也盖住整个窗口，能退出";
});

await rec.step(p, "弹到独立窗口：主窗口的桌面窗格关掉、侧栏出现标识；再打开桌面不新开窗格；关掉独立窗口后标识消失", async () => {
  if (!vnc) return "跳过";
  await u.click('.desk__bar button[aria-label="Open in separate window"]');
  try {
    await u.waitFor(`!document.querySelector('.desk')`, "主窗口的桌面窗格关掉", 15000);
  } catch (e) {
    // 带上定位信息：页面报错、窗格上的提示、现有窗口
    const note = await p.js(`document.querySelector('.desk__note')?.innerText || ''`).catch(() => "");
    const wins = await p.js(`window.__TAURI_INTERNALS__.invoke('plugin:window|get_all_windows')`).catch((x) => String(x));
    throw new Error(`${e.message}｜提示：${note}｜窗口：${JSON.stringify(wins)}｜页面错误：${p.errors.slice(-5).join(" / ")}`);
  }
  await u.waitFor(`!!document.querySelector('.srv-row__desk')`, "侧栏里的“桌面在独立窗口里”标识", 8000);
  const labels = await p.js(`window.__TAURI_INTERNALS__.invoke('plugin:window|get_all_windows')`);
  const win = (labels ?? []).find((l) => l.startsWith("desktop-"));
  if (!win) throw new Error(`没有独立桌面窗口：${JSON.stringify(labels)}`);
  await openDesktop();
  await sleep(3000);
  if (await p.js(`!!document.querySelector('.desk')`)) throw new Error("已经弹到独立窗口了，再打开桌面不该在主窗口新开窗格");
  const again = await p.js(`window.__TAURI_INTERNALS__.invoke('plugin:window|get_all_windows')`);
  if ((again ?? []).filter((l) => l.startsWith("desktop-")).length !== 1) throw new Error(`独立窗口应该只有一个：${JSON.stringify(again)}`);
  await p.js(`window.__TAURI_INTERNALS__.invoke('plugin:window|close', { label: ${JSON.stringify(win)} })`);
  await u.waitFor(`!document.querySelector('.srv-row__desk')`, "关掉独立窗口后标识消失", 15000);
  // 后面的步骤还要在主窗口里用这个桌面：重新打开
  await openDesktop();
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected') || document.querySelectorAll('.desk__row').length > 0`, "主窗口里重新打开桌面", 30000);
  return `独立窗口 ${win}`;
});

await rec.step(p, "关闭桌面窗格：服务器上的两个桌面照常运行", async () => {
  if (!vnc) return "跳过";
  // 只看终端区的标签（顶部导航也是 role=tab，不能混在一起数）
  await u.click(".tab[aria-selected=true] .tab__close");
  await sleep(1500);
  if (await p.js(`!!document.querySelector('.desk')`)) throw new Error("桌面窗格没关掉");
  const ps = execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "pl", "--", "pgrep", "-c", "-x", "Xtigervnc"]).toString().trim();
  if (Number(ps) < 2) throw new Error(`服务器上的桌面少了：${ps}`);
  return `服务器上 Xtigervnc 进程数：${ps}`;
});

await rec.step(p, "设置里打开“在独立窗口打开”：从菜单打开桌面直接开独立窗口，主窗口不开窗格；关掉设置后恢复", async () => {
  if (!vnc) return "跳过";
  const toggle = async (on) => {
    await u.clickText("Settings", "nav");
    await u.clickText("Server desktop", ".settings__nav");
    const sel = `[role=switch][aria-label="Open in a separate window"]`;
    await u.waitFor(`!!document.querySelector(${JSON.stringify(sel)})`, "设置里的“在独立窗口打开”");
    if ((await p.js(`document.querySelector(${JSON.stringify(sel)}).getAttribute('aria-checked')`)) !== String(on)) await u.click(sel);
    await u.waitFor(`document.querySelector(${JSON.stringify(sel)})?.getAttribute('aria-checked')===${JSON.stringify(String(on))}`, "开关切换好");
    await u.clickText("Workspace", "nav");
    await sleep(600);
  };
  const desktopWindow = async () => ((await p.js(`window.__TAURI_INTERNALS__.invoke('plugin:window|get_all_windows')`)) ?? []).find((l) => l.startsWith("desktop-")) ?? null;
  await toggle(true);
  let win = null;
  try {
    await openDesktop();
    const deadline = Date.now() + 20000;
    while (!(win = await desktopWindow()) && Date.now() < deadline) await sleep(500);
    if (!win) throw new Error("没有开出独立桌面窗口");
    await sleep(2000);
    if (await p.js(`!!document.querySelector('.desk')`)) throw new Error("开了“在独立窗口打开”，主窗口不该再开桌面窗格");
    await u.waitFor(`!!document.querySelector('.srv-row__desk')`, "侧栏里的“桌面在独立窗口里”标识", 8000);
    await p.js(`window.__TAURI_INTERNALS__.invoke('plugin:window|close', { label: ${JSON.stringify(win)} })`);
    await u.waitFor(`!document.querySelector('.srv-row__desk')`, "关掉独立窗口后标识消失", 15000);
  } finally {
    await toggle(false);
  }
  return `直接开了独立窗口 ${win}`;
});

/** WSL 里用户 pl 的 Xtigervnc 进程数 */
const xvncCount = () => {
  try { return Number(execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "pl", "--", "pgrep", "-c", "-x", "Xtigervnc"]).toString().trim()); } catch { return 0; }
};
/** WSL 里 Pierlink 桌面的状态文件个数 */
const stateFiles = () => {
  try { return Number(execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "pl", "--", "sh", "-c", "ls ~/.local/state/pierlink/vnc/*.json 2>/dev/null | wc -l"]).toString().trim()); } catch { return -1; }
};

await rec.step(p, "新建 Pierlink 桌面：选 Openbox 新建，直接连上；关闭桌面后服务器上的进程和记录都没了", async () => {
  if (!vnc) return "跳过";
  const before = xvncCount();
  await openDesktop();
  // 上次连的是 :5（记住了密码），会直接连上：先断开，再“选择其他桌面”回到列表
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected') || !!document.querySelector('.desk__create')`, "连上或出现新建桌面", 30000);
  if (await p.js(`!!document.querySelector('[aria-label="Disconnect"]')`)) {
    await u.click('[aria-label="Disconnect"]');
    await u.clickText("Choose another desktop", ".desk__card");
  }
  await u.waitFor(`!!document.querySelector('.desk__create')`, "新建桌面", 30000);
  // 有多个桌面环境时选 Openbox
  const picked = await p.js(`(()=>{const s=document.querySelector('.desk__create select'); if(!s) return 'only'; const o=[...s.options].find(o=>/openbox/i.test(o.value+o.textContent)); if(!o) return null; s.value=o.value; s.dispatchEvent(new Event('change',{bubbles:true})); return o.value;})()`);
  if (picked === null) throw new Error("桌面环境里没有 Openbox");
  await u.clickText("Create desktop", ".desk__create");
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected')`, "新桌面连上", 90000);
  await u.waitFor(`!!document.querySelector('[aria-label="Close desktop"]')`, "关闭桌面按钮");
  const during = xvncCount();
  if (during !== before + 1) throw new Error(`新建后 Xtigervnc 进程数不对：之前 ${before}，现在 ${during}`);
  const status = await p.js(`document.querySelector('.desk__status')?.innerText || ''`);
  await u.click('[aria-label="Close desktop"]');
  await u.clickText("Close desktop", ".pl-dialog");
  await u.waitFor(`document.querySelectorAll('.desk__row').length >= 2 && !!document.querySelector('.desk__create')`, "关闭后回到列表", 30000);
  let after = xvncCount();
  for (let i = 0; i < 20 && after !== before; i++) { await sleep(500); after = xvncCount(); }
  if (after !== before) throw new Error(`关闭后进程还在：之前 ${before}，现在 ${after}`);
  const left = stateFiles();
  if (left !== 0) throw new Error(`关闭后还留着 ${left} 个状态文件`);
  return `${status}；新建前后进程数 ${before} → ${during} → ${after}`;
});

p.close();
killApp(EXE);
if (vnc) {
  vnc.kill();
  try { execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "root", "--", "pkill", "-x", "Xtigervnc"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
}
if (wslSshd) {
  try { execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "root", "--", "pkill", "-x", "sshd"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
  wslSshd.kill();
}
process.exitCode = rec.finish() ? 1 : 0;
