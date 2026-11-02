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

/** :5 桌面的 X 剪贴板（CLIPBOARD）里的内容（默认取文字，target 取指定格式）；读不到时为空 */
const remoteClipboard = (target = "") => {
  try {
    return execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "pl", "--", "sh", "-c", `DISPLAY=:5 XAUTHORITY="$HOME/.vnc-e2e/x5.auth" timeout 5 xclip -o -selection clipboard${target ? ` -t ${target}` : ""} 2>/dev/null`], { encoding: "utf8", timeout: 15000 });
  } catch {
    return "";
  }
};
/** 等远程剪贴板的文字变成 want */
const waitRemote = async (want, what, timeoutMs = 10000) => {
  const deadline = Date.now() + timeoutMs;
  let got = "";
  while (Date.now() < deadline) {
    got = remoteClipboard();
    if (got === want) return;
    await sleep(500);
  }
  throw new Error(`${what}：远程剪贴板是 ${JSON.stringify(got.slice(0, 80))}，应该是 ${JSON.stringify(want)}`);
};
/** 设 Windows 剪贴板（经 base64 传进去，中文不受控制台代码页影响） */
const setWindowsClipboard = (text) => {
  const b64 = Buffer.from(text, "utf8").toString("base64");
  execFileSync("powershell.exe", ["-NoProfile", "-Command", `Set-Clipboard -Value ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('${b64}')))`]);
};
/** 读 Windows 剪贴板的文字（经 base64 传出来） */
const getWindowsClipboard = () => {
  const b64 = execFileSync("powershell.exe", ["-NoProfile", "-Command", "$t = Get-Clipboard -Raw; if ($null -eq $t) { $t = '' }; [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($t))"], { encoding: "utf8" }).trim();
  return Buffer.from(b64, "base64").toString("utf8");
};
/**
 * 在 :5 桌面里复制一段文字：xclip 在后台一直占着剪贴板，直到别人换掉它。
 * 不能同步等 wsl.exe（后台的 xclip 会让它一直不返回，execFileSync 的超时也拦不住）；
 * xclip 用 setsid 完全脱离这次 wsl 会话，再反复读，直到远程剪贴板真的是这段内容（WSL 起会话可能要好几秒）
 */
const copyOnRemote = async (text, target = "") => {
  const file = `/tmp/pl-clip-${Date.now()}.txt`;
  // xclip 留在前台占着剪贴板（-quiet），这次 wsl.exe 就一直开着：新版 WSL 在 wsl.exe 退出时会结束它启动的后台进程，
  // 用 setsid -f 放到后台的 xclip 跟着没了，非文字内容就读不回来。别的程序接管剪贴板后 xclip 自己退出
  spawn("wsl.exe", ["-d", LX.wsl, "-u", "pl", "--", "sh", "-c", `printf %s '${text}' > ${file} && DISPLAY=:5 XAUTHORITY="$HOME/.vnc-e2e/x5.auth" exec xclip -quiet -selection clipboard${target ? ` -t ${target}` : ""} -i ${file} </dev/null >/dev/null 2>&1`], { stdio: "ignore" }).on("error", () => undefined);
  const deadline = Date.now() + 15000;
  let got = "";
  while (Date.now() < deadline) {
    if ((got = remoteClipboard(target)) === text) return;
    await sleep(500);
  }
  throw new Error(`没能在远程剪贴板里放进 ${JSON.stringify(text)}${target ? `（${target}）` : ""}：读到 ${JSON.stringify(got.slice(0, 80))}`);
};

await rec.step(p, "在桌面画面里按 Ctrl+V：远程剪贴板收到 Windows 剪贴板的文字；断开重连后同样的文字也能再发过去", async () => {
  if (!vnc) return "跳过";
  setWindowsClipboard("PLDESKPASTE");
  const paste = async () => {
    await u.click(".desk__screen canvas");
    await sleep(300);
    await u.key("v", "KeyV", 86, 2);
  };
  await paste();
  await waitRemote("PLDESKPASTE", "按 Ctrl+V 后");
  await u.click('[aria-label="Disconnect"]');
  await u.clickText("Reconnect", ".desk__card");
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected')`, "重新连上", 30000);
  await sleep(1000);
  await paste();
  await waitRemote("PLDESKPASTE", "重连后再按 Ctrl+V");
  return "两次远程剪贴板都收到了";
});

await rec.step(p, "剪贴板自动同步：Windows 复制后不按 Ctrl+V，点一下画面远程就有了（窗口在前台时不点也会同步）；远程复制了非文字的内容后，Windows 剪贴板里的旧文字不会冲掉它", async () => {
  if (!vnc) return "跳过";
  setWindowsClipboard("PLDESKAUTO");
  await u.click(".desk__screen canvas");
  await waitRemote("PLDESKAUTO", "复制后点一下画面");
  let polled = "窗口不在前台，没测不点画面的同步";
  if (await p.js(`document.hasFocus()`)) {
    setWindowsClipboard("PLDESKPOLL");
    await waitRemote("PLDESKPOLL", "复制后不点画面");
    polled = "不点画面也同步了";
  }
  // 远程剪贴板里放一段非文字的内容（像在远程复制了文件）
  await copyOnRemote("PLRICH", "text/x-pierlink");
  await u.click(".desk__screen canvas");
  await sleep(300);
  await u.key("v", "KeyV", 86, 2);
  await sleep(2500);
  const rich = remoteClipboard("text/x-pierlink");
  if (rich !== "PLRICH") throw new Error(`点画面、按 Ctrl+V 后，远程剪贴板里的非文字内容被冲掉了：${JSON.stringify(rich.slice(0, 80))}`);
  return `点画面后同步了；${polled}；远程的非文字内容没被冲掉`;
});

await rec.step(p, "剪贴板：远程复制的文字到了 Windows；Windows 换过别的再复制回这段文字，远程照样收到；剪贴板里是中文时重新连接，远程收到的是中文", async () => {
  if (!vnc) return "跳过";
  await copyOnRemote("PLFROMREMOTE");
  const deadline = Date.now() + 10000;
  let got = "";
  while (Date.now() < deadline && (got = getWindowsClipboard()) !== "PLFROMREMOTE") await sleep(500);
  if (got !== "PLFROMREMOTE") throw new Error(`远程复制后 Windows 剪贴板是 ${JSON.stringify(got.slice(0, 80))}，应该是 "PLFROMREMOTE"`);
  setWindowsClipboard("PLOTHER");
  await u.click(".desk__screen canvas");
  await waitRemote("PLOTHER", "Windows 复制别的以后");
  setWindowsClipboard("PLFROMREMOTE");
  await u.click(".desk__screen canvas");
  await waitRemote("PLFROMREMOTE", "Windows 再复制回远程来过的文字以后");
  setWindowsClipboard("剪贴板中文 PL");
  await u.click('[aria-label="Disconnect"]');
  await u.clickText("Reconnect", ".desk__card");
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected')`, "重新连上", 30000);
  await waitRemote("剪贴板中文 PL", "剪贴板里是中文时重新连接以后");
  return "远程到 Windows、复制回来、中文重连都对";
});

await rec.step(p, "剪贴板：远程复制的文字到了 Windows 后断开重连，不发回远程（远程剪贴板仍归复制它的程序，文件管理器复制的文件不会变成路径）", async () => {
  if (!vnc) return "跳过";
  await copyOnRemote("PLKEEPOWNER", "UTF8_STRING");
  const deadline = Date.now() + 10000;
  let got = "";
  while (Date.now() < deadline && (got = getWindowsClipboard()) !== "PLKEEPOWNER") await sleep(500);
  if (got !== "PLKEEPOWNER") throw new Error(`远程复制后 Windows 剪贴板是 ${JSON.stringify(got.slice(0, 80))}，应该是 "PLKEEPOWNER"`);
  await u.click('[aria-label="Disconnect"]');
  await u.clickText("Reconnect", ".desk__card");
  await u.waitFor(`(document.querySelector('.desk__status')?.innerText||'').startsWith('Connected')`, "重新连上", 30000);
  // 等过扩展剪贴板的等待（最多 1.5 秒）和每秒一次的检查，再点一下画面（也会看一次剪贴板），留出发过去的时间
  await sleep(2500);
  await u.click(".desk__screen canvas");
  await sleep(2000);
  // xclip 只提供 TARGETS 和 UTF8_STRING；Xvnc 接管后会多出 TIMESTAMP、STRING、TEXT
  const targets = remoteClipboard("TARGETS");
  if (/\bTIMESTAMP\b/.test(targets)) throw new Error(`重连后 Pierlink 把文字发回了远程，剪贴板被 Xvnc 接管（TARGETS：${JSON.stringify(targets.slice(0, 200))}）`);
  if (!/\bUTF8_STRING\b/.test(targets)) throw new Error(`重连后读不到远程剪贴板的格式（TARGETS：${JSON.stringify(targets.slice(0, 200))}）`);
  return `远程剪贴板仍归 xclip：${targets.replace(/\s+/g, " ").trim()}`;
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

/**
 * WSL 里以 root 运行一段脚本（准备、还原用）。用 -e 直接执行：用 -- 时命令行先经过一层 shell，
 * 脚本里的 $f 这类变量在外层就被展开成空（会话文件没挪走，root 照样看得到桌面环境）
 */
const asRoot = (script, timeout = 60000) => execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "root", "-e", "sh", "-c", script], { encoding: "utf8", timeout });
/** 添加服务器对话框里填一项 */
async function fill(label, value) {
  const ok = await p.js(`(()=>{const l=[...document.querySelectorAll('.pl-dialog label.pl-field')].find(l=>l.firstChild?.textContent?.trim()===${JSON.stringify(label)} || l.textContent.trim().startsWith(${JSON.stringify(label)})); const i=l?.querySelector('input,textarea'); if(!i) return false; i.focus(); i.select(); return true;})()`);
  if (!ok) throw new Error(`找不到输入框：${label}`);
  await u.type(value);
}
/** 画布上 8×8 个点里有几种颜色（远程桌面画出东西来了就不止一种） */
const canvasColors = () => p.js(`(()=>{const c=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').map(s=>s.querySelector('.desk__screen canvas')).find(Boolean); if(!c||!c.width) return 0; const g=c.getContext('2d'); const seen=new Set(); for(let i=1;i<=8;i++) for(let j=1;j<=8;j++){const d=g.getImageData(Math.floor(c.width*i/9), Math.floor(c.height*j/9), 1, 1).data; seen.add(d[0]+','+d[1]+','+d[2]);} return seen.size;})()`);

/** 看得见的桌面窗格（隐藏的标签里也可能有别的桌面窗格，选择器都限定在这一个里） */
const deskPane = `[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').map(s=>s.querySelector('.desk')).find(Boolean)`;
const inDesk = (sel) => `${deskPane}?.querySelector(${JSON.stringify(sel)})`;
/** 点看得见的桌面窗格里的按钮（text 给出时按文字找） */
async function clickInDesk(sel, text) {
  const ok = await p.js(`(()=>{const b=[...(${deskPane}?.querySelectorAll(${JSON.stringify(sel)})||[])].find(e=>${text ? `e.textContent.trim()===${JSON.stringify(text)}` : "true"}); if(!b) return false; b.click(); return true;})()`);
  if (!ok) throw new Error(`桌面窗格里找不到：${sel}${text ? ` “${text}”` : ""}`);
}
const visiblePanes = `[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')`;
/** 装桌面的进度：开始安装了、装完了（后面的步骤据此决定跑不跑） */
const install = { started: false, done: false, prepared: false };

await rec.step(p, "安装桌面（一）：没有桌面环境的服务器上点“安装桌面”，检查完在下面的终端里自动输入安装命令（root 登录，命令不带 sudo）", async () => {
  if (!vnc) return "跳过";
  const key = process.env.PIERLINK_TEST_LINUX_KEY;
  if (!key) return "跳过：没有 PIERLINK_TEST_LINUX_KEY";
  // 上一步的桌面窗格还开着：先关掉这个标签，免得点到它的按钮
  for (let i = 0; i < 3 && (await p.js(`!!${deskPane}`)); i++) {
    await u.click(".tab[aria-selected=true] .tab__close");
    await sleep(1500);
  }
  if (await p.js(`!!${deskPane}`)) throw new Error("前面的桌面窗格没关掉");
  // root 用同一把测试密钥登录（sshd 默认允许 root 用密钥登录）；把会话文件（openbox）挪开，root 看到的就是“没有桌面环境”
  install.prepared = true;
  asRoot("install -d -m 700 /root/.ssh && install -m 600 /home/pl/.ssh/authorized_keys /root/.ssh/authorized_keys && mkdir -p /root/pl-e2e-xsessions && for f in /usr/share/xsessions/*.desktop; do [ -e \"$f\" ] && mv \"$f\" /root/pl-e2e-xsessions/; done; true");
  await p.js(`document.querySelector('[aria-label="Hide panel"]')?.click()`);
  await u.click('[aria-label="Add server"]');
  await u.waitFor(`!!document.querySelector('.pl-dialog')`, "添加服务器对话框");
  await fill("Host", LX.host);
  await fill("Port", LX.port);
  await fill("User", "root");
  await fill("Display name", "pl-root");
  await u.clickText("Specific key", ".pl-dialog");
  await fill("Private key file", key);
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes('pl-root')); return i>=0 ? String(i) : null;})()`, "侧栏里的 pl-root");
  await u.click(".srv-row", Number(row), "right");
  await u.clickText("Open desktop", ".pl-menu");
  // 同一个 localhost:2222，指纹前面已经信任过；万一再问就信任
  await u.waitFor(`/Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'') || !!${deskPane}`, "指纹确认或桌面窗格", 30000);
  if (await p.js(`/Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'')`)) await u.clickText("Trust and connect", ".pl-dialog");
  await u.waitFor(`!!${inDesk(".desk__install")}`, "“安装桌面”的卡片（root 看不到会话文件）", 60000);
  await clickInDesk(".desk__install .pl-btn--primary");
  const state = await u.waitFor(`(()=>{const s=document.querySelector('.desk-install')?.dataset.state; return s && s!=='checking' ? s : null;})()`, "安装前检查做完", 120000);
  const dialog = await p.js(`document.querySelector('.pl-dialog')?.innerText || ''`);
  if (state !== "ready") throw new Error(`检查结果不能装（${state}）：${dialog}`);
  // root 登录：命令不带 sudo
  const command = await p.js(`document.querySelector('.desk-install .mcp-cmd code')?.textContent || ''`);
  if (!/^apt-get update && env DEBIAN_FRONTEND=noninteractive apt-get install -y -o Dpkg::Progress-Fancy=1 --no-install-recommends .*xfce4 /.test(command)) throw new Error(`安装命令不对：${command}`);
  await u.clickText("Start install", ".pl-dialog");
  await u.waitFor(`${visiblePanes}.length >= 2`, "桌面下面开出一个终端", 30000);
  await u.waitFor(`/Installing desktop/.test(${inDesk(".desk__card")}?.innerText||'')`, "桌面卡片变成“正在安装”", 15000);
  await u.waitFor(`${visiblePanes}.some(s=>/apt-get update/.test(s.querySelector('.xterm-rows')?.innerText||''))`, "终端里输入了安装命令", 60000);
  // 卡片上显示阶段进度（更新软件源 / 下载第几个包 / 安装百分比）和终端最后一行
  await u.waitFor(`/Updating package lists|Downloading packages|Installing/.test(${inDesk(".desk__card")}?.innerText||'') && !!(${inDesk(".desk__install-line")}?.textContent||'').trim()`, "卡片上的安装进度", 120000);
  install.started = true;
  return `检查结果：${dialog.replace(/\s+/g, " ").slice(0, 200)}`;
});

await rec.step(p, "安装桌面（二）：在终端里装好 XFCE，桌面卡片自动回到“新建桌面”并选中 XFCE", async () => {
  if (!install.started) return "跳过：上一步没开始安装";
  // 软件源下载一百多 MB、装一百多个包：一步最多 10 分钟，这里等 9 分钟
  try {
    // 会话文件只剩 XFCE 一个时不显示选择框
    await u.waitFor(`!!${inDesk(".desk__create")} && !${inDesk(".desk__install")} && !/Installing desktop/.test(${inDesk(".desk__card")}?.innerText||'') && (!${inDesk(".desk__create [role=combobox]")} || /Xfce/i.test(${inDesk(".desk__create")}?.innerText||''))`, "装好后回到新建卡片并选中 XFCE", 9 * 60000);
  } catch (e) {
    const term = await p.js(`${visiblePanes}.map(s=>s.querySelector('.xterm-rows')?.innerText||'').join('\\n---\\n')`);
    throw new Error(`${e.message}｜终端最后几行：${term.split("\n").filter(Boolean).slice(-15).join(" / ")}`);
  }
  install.done = true;
  const tail = await p.js(`${visiblePanes}.map(s=>s.querySelector('.xterm-rows')?.innerText||'').join('\\n')`);
  return `装好了｜终端最后一行：${tail.split("\n").filter(Boolean).slice(-1)[0] ?? ""}`;
});

await rec.step(p, "安装桌面（三）：新建 XFCE 桌面，画面不是纯色；关掉后进程没了", async () => {
  if (!install.done) return "跳过：没装好";
  await clickInDesk(".desk__create button", "Create desktop");
  await u.waitFor(`(${inDesk(".desk__status")}?.innerText||'').startsWith('Connected')`, "XFCE 桌面连上", 180000);
  // XFCE 起来要几秒（面板、桌面背景）：等画面上出现多种颜色
  let colors = 0;
  const deadline = Date.now() + 90000;
  while (Date.now() < deadline && (colors = await canvasColors()) < 3) await sleep(2000);
  if (colors < 3) throw new Error(`XFCE 桌面画面还是纯色（取样颜色数 ${colors}）`);
  const sessions = asRoot("ps -eo comm= | grep -cE '^(xfce4-session|xfwm4|xfce4-panel|Xtigervnc)$' || true").trim();
  await clickInDesk('[aria-label="Close desktop"]');
  await u.clickText("Close desktop", ".pl-dialog");
  return `画面取样颜色 ${colors} 种｜XFCE 相关进程 ${sessions} 个`;
});

await rec.step(p, "安装桌面：还原 WSL 里挪走的会话文件", async () => {
  if (!install.prepared) return "跳过";
  asRoot("mv /root/pl-e2e-xsessions/*.desktop /usr/share/xsessions/ 2>/dev/null; rmdir /root/pl-e2e-xsessions 2>/dev/null; true");
  return "已还原";
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
