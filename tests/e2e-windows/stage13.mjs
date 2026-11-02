// 快捷键的 Windows 端到端测试：焦点在终端里按快捷键新建标签、开关文件栏、切标签（这些键不进终端）、开关服务器概况、关窗格；
// 设置 → 快捷键里改成和别的动作相同的组合键时提示冲突、不保存；改成新的组合键后新键生效、旧键失效；最后全部恢复默认。
// 在 run.mjs 之后运行：那里已经添加了“服务器”127.0.0.1（本机 Windows sshd，私钥登录）并信任了主机指纹。
//
// 环境变量：PIERLINK_EXE、E2E_OUT（必填）；PIERLINK_TEST_WIN_SSHD_KEY（没有时跳过概况那一步）。
// 本地调试：E2E_PREVIEW_URL 指向浏览器预览（vite），E2E_CDP_PORT 为外面先启动的 Chrome 的调试端口（默认 9222）；
// 预览里的终端只回显，不检查 PowerShell 补全，服务器用示例数据里的 gpu-dev。
// 每一步截图；某一步失败不中断后面的步骤。全部通过时退出码为 0。
import { join } from "node:path";
import { connect, killApp, launchApp, recorder, sleep, ui } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const PREVIEW = process.env.E2E_PREVIEW_URL;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, "快捷键");
const HAVE_SSHD = !!process.env.PIERLINK_TEST_WIN_SSHD_KEY || !!PREVIEW;
const PORT = Number(process.env.E2E_CDP_PORT || 9222);
if ((!EXE && !PREVIEW) || !OUT) throw new Error("缺少环境变量 PIERLINK_EXE 或 E2E_OUT");
/** 用来测概况的服务器（侧栏里按地址找） */
const SERVER = PREVIEW ? "203.0.113.18" : "127.0.0.1";

/** 修饰键：Ctrl=2，Shift=8 */
const CTRL = 2;
const CTRL_SHIFT = 2 | 8;

const rec = recorder(OUT);

/** 启动 Pierlink（或连上预览页） */
async function launch() {
  if (PREVIEW) {
    const p = await connect(PORT, 30000);
    await p.send("Emulation.setDeviceMetricsOverride", { width: 1400, height: 900, deviceScaleFactor: 1, mobile: false });
    await p.send("Page.navigate", { url: PREVIEW });
    await sleep(2500);
    const u = ui(p);
    await u.waitFor(`!!document.querySelector('.tabbar')`, "工作区加载完成", 30000);
    return { p, u };
  }
  killApp(EXE);
  await sleep(1000);
  return launchApp(EXE, OUT, PORT);
}

let { p, u } = await launch();
const conn = () => p;

/** 可见的窗格（当前标签里的） */
const VISIBLE = `[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')`;
/** 第一个可见窗格里终端的文字 */
const TERM_TEXT = `(${VISIBLE}[0]?.querySelector('.xterm-rows')?.innerText||'')`;
/** 第一个可见窗格的文件栏按钮 */
const FILES_BTN = `${VISIBLE}[0]?.querySelector('.pane__btn[aria-label="Files (current directory)"]')`;
const filesOpen = () => p.js(`${FILES_BTN}?.getAttribute('aria-pressed')==='true' && !!${VISIBLE}[0]?.querySelector('.files')`);
const filesClosed = `${FILES_BTN}?.getAttribute('aria-pressed')==='false' && !${VISIBLE}[0]?.querySelector('.files')`;
const tabCount = () => p.js(`document.querySelectorAll('.tab').length`);
const ACTIVE_TAB = `[...document.querySelectorAll('.tab')].findIndex(t=>t.getAttribute('aria-selected')==='true')`;
/** 设置里某个动作的按键、这一行的提示 */
const keyBtn = (id) => `document.querySelector('[data-shortcut="${id}"] .sc-key')`;
const noteOf = (id) => `(document.querySelector('[data-shortcut="${id}"] .sc-row__note')?.textContent||'')`;
/** 设置里存的快捷键改动 */
const savedShortcuts = () => p.js(`JSON.stringify(JSON.parse(localStorage.getItem('pierlink.settings.v1')||'{}').shortcuts ?? {})`);

/** 点一下当前标签里的终端，让焦点在终端里（快捷键要在终端里也能用） */
async function focusTerminal() {
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.waitFor(`document.activeElement?.classList.contains('xterm-helper-textarea') && ${VISIBLE}.some(s=>s.contains(document.activeElement))`, "焦点在终端里");
}
const press = (letter, modifiers) => u.key(letter, `Key${letter}`, letter.charCodeAt(0), modifiers);

/** 这次测试新开的本机终端标签（data-tab-key） */
let myTab = null;
async function toMyTab() {
  if (!myTab) throw new Error("前面没能新建标签");
  await u.click(`.tab[data-tab-key="${myTab}"]`);
  await u.waitFor(`document.querySelector('.tab[data-tab-key="${myTab}"]')?.getAttribute('aria-selected')==='true'`, "切到测试用的标签");
  await focusTerminal();
}
/** 等设置页滚动停下来：点左边分类是平滑滚动，滚动中按坐标点会点偏 */
async function settle() {
  let last = null;
  for (let i = 0; i < 40; i++) {
    const top = await p.js(`document.querySelector('.settings__main')?.scrollTop ?? 0`);
    if (top === last) return;
    last = top;
    await sleep(200);
  }
}
/** 打开设置 → 快捷键 */
async function openShortcutSettings() {
  await u.key(",", "Comma", 188, CTRL);
  await u.waitFor(`(document.querySelector('.settings')?.offsetWidth||0) > 0`, "Ctrl+, 打开设置页");
  await u.clickText("Shortcuts", ".settings__nav");
  await u.waitFor(`!!${keyBtn("toggleFiles")}`, "快捷键一节");
  await settle();
}
/** 点设置里某个动作的按键：先滚到正中、等滚动停下再点 */
async function clickKey(id) {
  await p.js(`${keyBtn(id)}.scrollIntoView({ block: 'center', behavior: 'instant' })`);
  await settle();
  await u.click(`[data-shortcut="${id}"] .sc-key`);
}

// ---------- 终端区的快捷键 ----------
await rec.step(conn, "焦点在终端里按 Ctrl+Shift+T：新建一个本机终端标签，焦点到新终端里", async () => {
  // 先把焦点放进现有的终端（有的话）：快捷键在终端里也要生效
  if (await p.js(`!!document.querySelector(".pane-slot:not([style*='none']) .xterm-screen")`)) await focusTerminal();
  const before = await tabCount();
  await press("T", CTRL_SHIFT);
  await u.waitFor(`document.querySelectorAll('.tab').length===${before + 1}`, "多了一个标签");
  await u.waitFor(`(()=>{const t=[...document.querySelectorAll('.tab')]; return t[t.length-1]?.getAttribute('aria-selected')==='true';})()`, "新标签是当前标签");
  await u.waitFor(`/PS |>|\\$ /.test(${TERM_TEXT})`, "新终端出现提示符", 30000);
  await u.waitFor(`document.activeElement?.classList.contains('xterm-helper-textarea') && ${VISIBLE}.some(s=>s.contains(document.activeElement))`, "焦点在新终端里");
  myTab = await p.js(`${VISIBLE}[0]?.dataset.paneTab`);
  return `标签 ${before} → ${before + 1}`;
});

await rec.step(conn, "焦点在终端里按 Ctrl+Shift+E：打开文件栏，再按一次收起", async () => {
  await toMyTab();
  if (await filesOpen()) throw new Error("新标签的文件栏应当是收起的");
  await press("E", CTRL_SHIFT);
  await u.waitFor(`${FILES_BTN}?.getAttribute('aria-pressed')==='true' && !!${VISIBLE}[0]?.querySelector('.files')`, "文件栏打开");
  await focusTerminal();
  await press("E", CTRL_SHIFT);
  await u.waitFor(filesClosed, "文件栏收起");
});

await rec.step(conn, "Ctrl+Tab、Ctrl+Shift+Tab 切到下一个、上一个标签，这两个键不进终端（PowerShell 没有补全）", async () => {
  if ((await tabCount()) < 2) {
    await press("T", CTRL_SHIFT);
    await u.waitFor(`document.querySelectorAll('.tab').length>=2`, "再开一个标签");
  }
  await toMyTab();
  const n = await tabCount();
  const a = await p.js(ACTIVE_TAB);
  // 输入一半的命令：Tab 或 Shift+Tab 进了 PowerShell 会补全成 Get-PSReadLineKeyHandler
  // （选一个没人敲过的命令：PowerShell 7 会按历史在后面显示浅色预测，常用命令可能被误认成补全）
  await u.type("Get-PSReadLineKeyHandl");
  await u.waitFor(`${TERM_TEXT}.replace(/\\n/g,'').includes('Get-PSReadLineKeyHandl')`, "终端里出现输入的文字");
  await u.key("Tab", "Tab", 9, CTRL);
  await u.waitFor(`${ACTIVE_TAB}===${(a + 1) % n}`, "切到下一个标签");
  await u.key("Tab", "Tab", 9, CTRL_SHIFT);
  await u.waitFor(`${ACTIVE_TAB}===${a}`, "切回原来的标签");
  await u.waitFor(`document.activeElement?.classList.contains('xterm-helper-textarea') && ${VISIBLE}.some(s=>s.contains(document.activeElement))`, "切回来后焦点在终端里");
  await sleep(1500);
  // 去掉换行再找：窗格窄时一行命令会折到下一行
  const text = (await p.js(TERM_TEXT)).replace(/\n/g, "");
  // 清掉输入行（Ctrl+C 是保留键，照常发给 PowerShell）
  await press("C", CTRL);
  if (!PREVIEW && /Get-PSReadLineKeyHandler/.test(text)) throw new Error("Ctrl+Tab 或 Ctrl+Shift+Tab 进了终端：PowerShell 做了补全");
  if (!text.includes("Get-PSReadLineKeyHandl")) throw new Error(`输入行不对：${text.slice(-200)}`);
  return `共 ${n} 个标签，从第 ${a + 1} 个切到第 ${((a + 1) % n) + 1} 个再切回来`;
});

await rec.step(conn, "Ctrl+Shift+L 把焦点给服务器搜索框；焦点在输入框里时 Ctrl+Shift+T 不新建标签", async () => {
  await toMyTab();
  await press("L", CTRL_SHIFT);
  await u.waitFor(`document.activeElement?.matches('.srv-search input')`, "焦点到搜索框");
  const before = await tabCount();
  await press("T", CTRL_SHIFT);
  await sleep(800);
  const after = await tabCount();
  if (after !== before) throw new Error(`焦点在搜索框里时新建了标签：${before} → ${after}`);
  await u.esc();
});

await rec.step(conn, "服务器终端里按 Ctrl+Shift+O 收起、再按显示概况；本机终端里按提示没有概况；Ctrl+Shift+W 关掉服务器标签", async () => {
  if (!HAVE_SSHD) throw new Error("跳过：没有本机 sshd");
  await toMyTab();
  await press("O", CTRL_SHIFT);
  await u.waitFor(`/no overview/.test(document.querySelector('.broadcast-notice')?.innerText||'')`, "本机终端里提示没有概况");
  const tabsBefore = await tabCount();
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes(${JSON.stringify(SERVER)})); return i>=0 ? String(i) : null;})()`, `侧栏里的 ${SERVER}`);
  await u.click(".srv-row", Number(row), "left", 2);
  await sleep(1500);
  if (await p.js(`/Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'')`)) await u.clickText("Trust and connect", ".pl-dialog");
  await u.waitFor(`document.querySelectorAll('.tab').length===${tabsBefore + 1}`, "服务器终端开在新标签里", 30000);
  const btn = `${VISIBLE}[0]?.querySelector('.pane__btn[aria-label="Show overview"]')`;
  await u.waitFor(`!!${btn}`, "服务器终端的概况按钮", 30000);
  // 等连上（出现提示符）再按：连上之前概况本来就不显示
  await u.waitFor(`/[>$#]\\s*$/m.test(${TERM_TEXT})`, "服务器终端出现提示符", 30000);
  const was = await p.js(`${btn}.getAttribute('aria-pressed')`);
  if (was === "true") await u.waitFor(`!!${VISIBLE}[0]?.querySelector('.ov')`, "连上后显示概况", 30000);
  await focusTerminal();
  const flipped = was === "true" ? "false" : "true";
  await press("O", CTRL_SHIFT);
  await u.waitFor(`${btn}?.getAttribute('aria-pressed')===${JSON.stringify(flipped)}`, "概况开关翻转");
  if (flipped === "false") await u.waitFor(`!${VISIBLE}[0]?.querySelector('.ov')`, "概况收起");
  else await u.waitFor(`!!${VISIBLE}[0]?.querySelector('.ov')`, "概况显示", 30000);
  await focusTerminal();
  await press("O", CTRL_SHIFT);
  await u.waitFor(`${btn}?.getAttribute('aria-pressed')===${JSON.stringify(was)}`, "再按一次恢复");
  if (was === "true") await u.waitFor(`!!${VISIBLE}[0]?.querySelector('.ov')`, "概况显示", 30000);
  // 关掉服务器标签：在跑程序时会问一下
  await focusTerminal();
  await press("W", CTRL_SHIFT);
  const closed = `document.querySelectorAll('.tab').length===${tabsBefore}`;
  const asked = await u.waitFor(`${closed} || !!document.querySelector('.pl-dialog')`, "关闭标签或确认框", 15000);
  if (asked && !(await p.js(closed))) await u.clickText("Close", ".pl-dialog");
  await u.waitFor(closed, "服务器标签关掉了", 15000);
  return `概况 ${was} → ${flipped} → ${was}`;
});

// ---------- 设置里改快捷键 ----------
await rec.step(conn, "设置 → 快捷键：把“显示/隐藏文件栏”改成概况在用的 Ctrl+Shift+O，提示冲突、不保存；保留键、只按 Ctrl 也不收；提示出现时按键不挪位置", async () => {
  await toMyTab();
  await openShortcutSettings();
  await u.waitFor(`${keyBtn("toggleFiles")}?.textContent==='Ctrl+Shift+E'`, "文件栏的快捷键是默认的 Ctrl+Shift+E");
  const pos = `(()=>{const r=${keyBtn("toggleFiles")}.getBoundingClientRect(); return [Math.round(r.x), Math.round(r.y), Math.round(r.width)].join(',');})()`;
  await clickKey("toggleFiles");
  await u.waitFor(`${keyBtn("toggleFiles")}?.getAttribute('aria-pressed')==='true'`, "进入录制");
  const at = await p.js(pos);
  await u.key("Control", "ControlLeft", 17, CTRL);
  await sleep(300);
  if ((await p.js(`${keyBtn("toggleFiles")}.getAttribute('aria-pressed')`)) !== "true") throw new Error("只按 Ctrl 就结束了录制");
  await press("V", CTRL);
  await u.waitFor(`/Ctrl\\+V is used for pasting in the terminal/.test(${noteOf("toggleFiles")})`, "保留键的提示");
  await press("O", CTRL_SHIFT);
  const msg = await u.waitFor(`(()=>{const t=${noteOf("toggleFiles")}; return /Ctrl\\+Shift\\+O is used by “Show or hide overview”/.test(t) ? t : null;})()`, "冲突提示写明是哪个动作");
  if ((await p.js(pos)) !== at) throw new Error(`提示出现后按键挪了位置：${at} → ${await p.js(pos)}`);
  await u.esc();
  await u.waitFor(`${keyBtn("toggleFiles")}?.getAttribute('aria-pressed')==='false'`, "Esc 结束录制");
  const text = await p.js(`${keyBtn("toggleFiles")}.textContent`);
  if (text !== "Ctrl+Shift+E") throw new Error(`冲突的键被用上了：${text}`);
  const saved = await savedShortcuts();
  if (saved !== "{}") throw new Error(`设置里存了：${saved}`);
  if ((await p.js(`${keyBtn("toggleOverview")}.textContent`)) !== "Ctrl+Shift+O") throw new Error("概况的快捷键被改了");
  return msg;
});

await rec.step(conn, "改成 Ctrl+Shift+Y：新键开关文件栏，旧键 Ctrl+Shift+E 不再起作用；按钮提示里是新键", async () => {
  await openShortcutSettings();
  await clickKey("toggleFiles");
  await u.waitFor(`${keyBtn("toggleFiles")}?.getAttribute('aria-pressed')==='true'`, "进入录制");
  await press("Y", CTRL_SHIFT);
  await u.waitFor(`${keyBtn("toggleFiles")}?.textContent==='Ctrl+Shift+Y' && ${keyBtn("toggleFiles")}.getAttribute('aria-pressed')==='false'`, "保存成 Ctrl+Shift+Y");
  const note = await p.js(noteOf("toggleFiles"));
  if (!/Default: Ctrl\+Shift\+E/.test(note)) throw new Error(`这一行应当写默认键：${note}`);
  const saved = await savedShortcuts();
  if (saved !== JSON.stringify({ toggleFiles: "Ctrl+Shift+KeyY" })) throw new Error(`设置里存的不对：${saved}`);
  await u.clickText("Workspace", "nav");
  await toMyTab();
  await press("E", CTRL_SHIFT);
  await sleep(800);
  if (await filesOpen()) throw new Error("旧键 Ctrl+Shift+E 还能打开文件栏");
  await press("Y", CTRL_SHIFT);
  await u.waitFor(`${FILES_BTN}?.getAttribute('aria-pressed')==='true' && !!${VISIBLE}[0]?.querySelector('.files')`, "新键打开文件栏");
  const tip = await p.js(`${FILES_BTN}?.getAttribute('data-pl-tip') || ${FILES_BTN}?.getAttribute('title') || ''`);
  if (!tip.includes("Ctrl+Shift+Y")) throw new Error(`文件栏按钮的提示里没有新键：${tip}`);
  await focusTerminal();
  await press("Y", CTRL_SHIFT);
  await u.waitFor(filesClosed, "新键收起文件栏");
  return tip;
});

await rec.step(conn, "全部恢复默认：设置里不再存快捷键，Ctrl+Shift+E 又能开关文件栏；Ctrl+Shift+W 关掉测试用的标签", async () => {
  await toMyTab();
  await openShortcutSettings();
  await p.js(`document.querySelector('.sc-head')?.scrollIntoView({ block: 'center', behavior: 'instant' })`);
  await settle();
  await u.clickText("Reset all", ".settings__main");
  await u.waitFor(`${keyBtn("toggleFiles")}?.textContent==='Ctrl+Shift+E'`, "恢复成 Ctrl+Shift+E");
  const saved = await savedShortcuts();
  if (saved !== "{}") throw new Error(`恢复默认后设置里还存着：${saved}`);
  await u.clickText("Workspace", "nav");
  await toMyTab();
  await press("E", CTRL_SHIFT);
  await u.waitFor(`${FILES_BTN}?.getAttribute('aria-pressed')==='true'`, "Ctrl+Shift+E 打开文件栏");
  await focusTerminal();
  await press("E", CTRL_SHIFT);
  await u.waitFor(filesClosed, "Ctrl+Shift+E 收起文件栏");
  const before = await tabCount();
  await press("W", CTRL_SHIFT);
  const closed = `document.querySelectorAll('.tab').length===${before - 1}`;
  await u.waitFor(`${closed} || !!document.querySelector('.pl-dialog')`, "关闭标签或确认框", 15000);
  if (!(await p.js(closed))) await u.clickText("Close", ".pl-dialog");
  await u.waitFor(closed, "测试用的标签关掉了", 15000);
});

if (p.errors.length) console.log("页面错误：", p.errors.join("\n"));
p.close();
if (!PREVIEW) killApp(EXE);
process.exitCode = rec.finish() ? 1 : 0;
