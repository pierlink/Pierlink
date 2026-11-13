// Windows 端到端测试：在 CI 的 Windows 机器上装好真实安装包后运行，像用户一样操作真实的 Pierlink。
//
// 环境变量：
//   PIERLINK_EXE                 安装后的 Pierlink.exe 完整路径（必填）
//   E2E_OUT                      截图和结果的输出目录（必填）
//   PIERLINK_TEST_WIN_SSHD_KEY   本机 sshd 的登录私钥（没有时跳过服务器相关步骤）
//   PIERLINK_TEST_WIN_SSHD_USER  本机 sshd 的登录用户
// 每一步截图；某一步失败不中断后面的步骤。全部通过时退出码为 0。
import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { blinkRhythm, blinkSteady, connect, killApp, launchApp, pwsh, recorder, sleep, spinnerCursor, ui } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const OUT = process.env.E2E_OUT;
const KEY = process.env.PIERLINK_TEST_WIN_SSHD_KEY;
const USER = process.env.PIERLINK_TEST_WIN_SSHD_USER;
const PORT = 9222;
if ((!EXE && !process.env.E2E_PREVIEW_URL) || !OUT) throw new Error("缺少环境变量 PIERLINK_EXE 或 E2E_OUT");

const HOME = homedir();
const rec = recorder(OUT);

/** 本地调试脚本用：E2E_PREVIEW_URL 指向浏览器预览，由外面先启动带调试端口的 Chrome */
const PREVIEW = process.env.E2E_PREVIEW_URL;

/** 启动 Pierlink（打开 WebView2 调试端口），返回连接 */
async function launch() {
  if (PREVIEW) {
    const p = await connect(PORT, 30000);
    await p.send("Page.navigate", { url: PREVIEW });
    await sleep(2500);
    const u = ui(p);
    await u.waitFor(`!!document.querySelector('.tabbar')`, "工作区加载完成", 30000);
    return { p, u };
  }
  return launchApp(EXE, OUT, PORT);
}

/** 强制结束 Pierlink（模拟崩溃或直接关机；工作区布局在变化后 0.4 秒内已保存） */
function kill() {
  if (PREVIEW) return;
  killApp(EXE);
}

function setClipboard(text) {
  execFileSync("powershell.exe", ["-NoProfile", "-Command", `Set-Clipboard -Value '${text}'`]);
}
/** 读 Windows 剪贴板（经 base64 传出来，中文不受控制台代码页影响） */
function getClipboard() {
  const b64 = execFileSync("powershell.exe", ["-NoProfile", "-Command", "$t = Get-Clipboard -Raw; if ($null -eq $t) { $t = '' }; [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($t))"], { encoding: "utf8" }).trim();
  return Buffer.from(b64, "base64").toString("utf8").replace(/\r?\n$/, "");
}
/** 等剪贴板变成 want（程序是异步写的） */
async function waitClipboard(want, what, timeoutMs = 8000) {
  const deadline = Date.now() + timeoutMs;
  let got = "";
  while (Date.now() < deadline) {
    got = getClipboard();
    if (got === want) return got;
    await sleep(400);
  }
  throw new Error(`${what}：剪贴板是 ${JSON.stringify(got.slice(0, 120))}，应该是 ${JSON.stringify(want)}`);
}
/** 当前可见的第一个终端里，word 最后一次出现的位置（屏幕坐标） */
const wordAt = (word) => p.js(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]; const rows=s?.querySelector('.xterm-rows'); if(!rows) return null; const w=document.createTreeWalker(rows, NodeFilter.SHOW_TEXT); let n, hit=null; while((n=w.nextNode())){ let i=n.textContent.indexOf(${JSON.stringify(word)}); while(i>=0){ hit=[n,i]; i=n.textContent.indexOf(${JSON.stringify(word)}, i+1); } } if(!hit) return null; const r=document.createRange(); r.setStart(hit[0],hit[1]); r.setEnd(hit[0],hit[1]+${JSON.stringify(word)}.length); const b=r.getBoundingClientRect(); return {x:b.x+b.width/2, y:b.y+b.height/2, left:b.x, right:b.x+b.width};})()`);
/**
 * 回到干净的提示符：点一下终端，按 Ctrl+C 取消输入行里没执行的文字，再按回车。
 */
async function freshPrompt() {
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.key("c", "KeyC", 67, 2);
  await sleep(300);
  await u.enter();
  await sleep(800);
}
/** 本机正在运行的 Edge 浏览器进程数（不算 WebView2） */
function edgeCount() {
  return Number(execFileSync("powershell.exe", ["-NoProfile", "-Command", "@(Get-Process msedge -ErrorAction SilentlyContinue).Count"], { encoding: "utf8" }).trim() || 0);
}

/** 当前可见的窗格数、标签数 */
const counts = (p) => p.js(`({ panes: [...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').length, tabs: document.querySelectorAll('.tab').length })`);

let { p, u } = await launch();

// ---------- 1. 基础流程 ----------
await rec.step(p, "打开默认本机终端：从用户主目录开始，能输入命令", async () => {
  if (!(await p.js(`document.querySelectorAll('.pane-slot').length`))) await u.clickText("Default local terminal");
  await u.waitFor(`(()=>{const r=document.querySelector('.xterm-rows'); return r && /PS |>/.test(r.innerText);})()`, "出现 PowerShell 提示符", 30000);
  // 不能继承 Pierlink 进程的目录（从开始菜单启动时是安装目录）
  const prompt = await u.waitFor(`(()=>{const m=document.querySelector('.xterm-rows').innerText.match(/PS (.+?)>/); return m && m[1];})()`, "提示符里的目录");
  if (prompt.toLowerCase() !== HOME.toLowerCase()) throw new Error(`起始目录不是用户主目录：${prompt}（应为 ${HOME}）`);
  await u.click(".xterm-screen");
  await u.typeLine("echo pl-e2e-hello");
  await u.waitFor(`/pl-e2e-hello\\s*\\n.*pl-e2e-hello/s.test(document.querySelector('.xterm-rows').innerText)`, "命令输出");
  return (await u.termText()).split("\n").filter(Boolean).slice(-3).join(" / ");
});

await rec.step(p, "终端光标在闪（系统要求减少动画时也闪），竖线默认 2 像素", async () => {
  await u.click(".xterm-screen");
  // 光标在闪：后半秒取到的是“灭”的样子（没有阴影），等到“亮”的时候再取（动画坏了也直接取，让下面报错）
  const r = await u.waitFor(`(()=>{const c=document.querySelector('.xterm-rows.xterm-focus .xterm-cursor'); if(!c) return null; const s=getComputedStyle(c); if(s.animationName!=='none' && !/2px 0px 0px 0px inset/.test(s.boxShadow)) return null; return {name:s.animationName,dur:s.animationDuration,count:s.animationIterationCount,shadow:s.boxShadow,reduced:matchMedia('(prefers-reduced-motion: reduce)').matches};})()`, "获得焦点的终端光标（亮的时候）", 8000);
  if (r.name === "none" || r.dur !== "1.2s" || r.count !== "infinite") throw new Error(`光标不闪或速度不对：${JSON.stringify(r)}`);
  if (!/2px 0px 0px 0px inset/.test(r.shadow)) throw new Error(`光标宽度不对：${r.shadow}`);
  return `系统要求减少动画=${r.reduced}；闪烁动画 ${r.dur} × ${r.count}`;
});

await rec.step(p, "闲着的 PowerShell：光标亮灭各 0.6 秒，节奏不乱", async () => {
  await u.click(".xterm-screen");
  await sleep(300);
  const r = await blinkRhythm(p, "document.querySelector('.xterm-rows')");
  if (!blinkSteady(r)) throw new Error(r.summary);
  return r.summary;
});

await rec.step(p, "程序不定时重画输入行（像 Codex、Claude Code 的状态栏）：光标照样亮灭各 0.6 秒，不被重画打乱", async () => {
  const js = join(OUT, "pl-redraw.js");
  writeFileSync(js, "const seq=[700,450,900,350,800,1000,500,650];let k=0;process.stdout.write('> abc');(function next(){setTimeout(()=>{process.stdout.write('\\r> abc\\x1b[K');if(++k<14)next();else process.stdout.write('\\r\\npl-redraw-done\\n');},seq[k%seq.length]);})();\n");
  await u.click(".xterm-screen");
  await u.typeLine(`& "${process.execPath}" "${js}"`);
  await u.waitFor(`/^> abc\\s*$/m.test(document.querySelector('.xterm-rows').innerText)`, "程序画出输入行", 15000);
  const r = await blinkRhythm(p, "document.querySelector('.xterm-rows')");
  await u.waitFor(`/^pl-redraw-done\\s*$/m.test(document.querySelector('.xterm-rows').innerText)`, "程序结束", 20000);
  rmSync(js, { force: true });
  if (r.redraws < 3) throw new Error(`程序没怎么重画，测不出来：${r.summary}`);
  if (!blinkSteady(r)) throw new Error(r.summary);
  return r.summary;
});

await rec.step(p, "终端失去焦点再回来（新版 ConPTY 打开了焦点上报）：PowerShell 输入行里不多出东西", async () => {
  await u.click(".xterm-screen");
  await p.js(`document.querySelector('.xterm-helper-textarea')?.blur()`);
  await sleep(400);
  await u.click(".xterm-screen");
  await sleep(400);
  await u.typeLine("echo pl-focus-ok");
  const text = await u.waitFor(`(()=>{const t=document.querySelector('.xterm-rows').innerText; return /^pl-focus-ok\\s*$/m.test(t) ? t : null;})()`, "命令输出", 10000);
  const line = text.split("\n").reverse().find((l) => l.includes("echo pl-focus-ok")) ?? "";
  if (/\[[IO]/.test(line)) throw new Error(`输入行里多了焦点上报：${line}`);
  return line.trim();
});

await rec.step(p, "程序把光标换成闪烁方块（\\e[1 q）：“灭”的半拍光标下的字是原来的颜色，不变成光标色；\\e[0 q 换回竖线", async () => {
  await u.typeLine('Write-Host -NoNewline "$([char]27)[1 q"');
  // 停住动画，分别取“亮”（0.3 秒）和“灭”（0.9 秒）时这一格的颜色
  const r = await u.waitFor(`(()=>{const c=document.querySelector('.xterm-rows.xterm-focus .xterm-cursor.xterm-cursor-blink'); if(!c || !c.classList.contains('pl-cursor-block')) return null; const a=c.getAnimations()[0]; if(!a) return null; a.pause(); const at=(t)=>{a.currentTime=t; const s=getComputedStyle(c); return {bg:s.backgroundColor,color:s.color};}; const on=at(300), off=at(900); a.play(); return {on, off, row:getComputedStyle(c.closest('.xterm-rows')).color, name:getComputedStyle(c).animationName, xtermBlock:c.classList.contains('xterm-cursor-block')};})()`, "闪烁的方块光标", 15000);
  if (r.name !== "pl-cursor-block" || r.xtermBlock) throw new Error(`方块光标没换成自己的样式：${JSON.stringify(r)}`);
  if (r.on.bg === "rgba(0, 0, 0, 0)" || r.on.color === r.on.bg) throw new Error(`“亮”的时候不是方块：${JSON.stringify(r)}`);
  if (r.off.color === r.on.bg) throw new Error(`“灭”的时候字变成了光标色：${JSON.stringify(r)}`);
  if (r.off.color !== r.row || r.off.bg !== "rgba(0, 0, 0, 0)") throw new Error(`“灭”的时候不是这一格原来的样子：${JSON.stringify(r)}`);
  await u.typeLine('Write-Host -NoNewline "$([char]27)[0 q"');
  await u.waitFor(`!!document.querySelector('.xterm-rows.xterm-focus .xterm-cursor.xterm-cursor-bar')`, "换回竖线光标", 15000);
  return `亮 ${r.on.bg} / ${r.on.color}；灭 ${r.off.bg} / ${r.off.color}`;
});

await rec.step(p, "终端用的是随安装包带的 ConPTY：控制台宿主是安装目录里的 OpenConsole.exe", async () => {
  const want = join(dirname(EXE), "conpty", "OpenConsole.exe").toLowerCase();
  const out = pwsh(`Get-CimInstance Win32_Process -Filter "Name='OpenConsole.exe'" | ForEach-Object { $_.ExecutablePath } | Sort-Object -Unique`);
  // 资源目录是 \\?\ 开头的写法，进程路径也带着它
  const running = out.split(/\r?\n/).map((x) => x.trim().replace(/^\\\\\?\\/, "").toLowerCase()).filter(Boolean);
  if (!running.includes(want)) throw new Error(`没有安装目录里的 OpenConsole.exe 在运行：${out || "（一个都没有）"}`);
  return out;
});

await rec.step(p, "转圈动画（和 npm install 一样分三次写：回行首、清行、写字符）时光标一直在转圈字符后面，不来回跳", async () => {
  // npm 的转圈：每 130 毫秒 cursorTo(0)、clearLine(1)、写一个字符，三次单独写到 stderr
  const js = join(OUT, "pl-spin.js");
  writeFileSync(js, "const s=process.stderr,f=['-','\\\\','|','/'];let i=0;s.write(f[0]);const t=setInterval(()=>{i=(i+1)%4;s.cursorTo(0);s.clearLine(1);s.write(f[i]);},130);setTimeout(()=>{clearInterval(t);s.cursorTo(0);s.clearLine(1);s.write('pl-spin-done\\n');},6000);\n");
  await u.click(".xterm-screen");
  await u.typeLine(`& "${process.execPath}" "${js}"`);
  const r = await spinnerCursor(p, "document.querySelector('.xterm-rows')");
  await u.waitFor(`/^pl-spin-done\\s*$/m.test(document.querySelector('.xterm-rows').innerText)`, "转圈结束", 15000);
  rmSync(js, { force: true });
  if (r.bad > 2 || r.badMs > 60) throw new Error(r.summary);
  return r.summary;
});

await rec.step(p, "切到设置页再切回来，终端和内容都还在", async () => {
  await u.clickText("Settings", "nav");
  await sleep(500);
  await u.clickText("Workspace", "nav");
  await sleep(500);
  const text = await u.termText();
  if (!text?.includes("pl-e2e-hello")) throw new Error(`终端内容丢了：${text}`);
});

/** 键盘焦点在看得见的终端里 */
const focusInVisibleTerminal = `(()=>{const a=document.activeElement; const s=a?.closest('.pane-slot'); return !!a && a.classList.contains('xterm-helper-textarea') && !!s && s.style.display!=='none';})()`;

await rec.step(p, "从设置页回到工作区：不用再点终端，直接能打字", async () => {
  await u.click(".xterm-screen");
  await u.clickText("Settings", "nav");
  await sleep(500);
  await u.clickText("Workspace", "nav");
  await u.waitFor(focusInVisibleTerminal, "焦点回到终端", 5000);
  await u.typeLine("echo pl-page-refocus");
  await u.waitFor(`/^pl-page-refocus\\s*$/m.test(document.querySelector('.pane-slot:not([style*="none"]) .xterm-rows')?.innerText||'')`, "命令进了终端", 10000);
  return "回到工作区后直接打字进了终端";
});

await rec.step(p, "从别的标签点回来：焦点回到这个标签的终端，直接能打字", async () => {
  const before = await counts(p);
  await u.click('.tabbar button[aria-label="New terminal tab"]');
  await u.waitFor(`document.querySelectorAll('.tab').length === ${before.tabs + 1}`, "第二个标签", 15000);
  await u.waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); return /PS |>/.test(s[0]?.querySelector('.xterm-rows')?.innerText||'');})()`, "新终端出现提示符", 30000);
  // 点回第一个标签，不点终端直接打字
  await u.click(".tab", 0);
  await u.waitFor(focusInVisibleTerminal, "焦点回到第一个标签的终端", 5000);
  await u.typeLine("echo pl-tab-refocus");
  await u.waitFor(`/^pl-tab-refocus\\s*$/m.test(document.querySelector('.pane-slot:not([style*="none"]) .xterm-rows')?.innerText||'')`, "命令进了第一个标签的终端", 10000);
  // 关掉第二个标签，标签数回到原样
  await u.click(".tab", before.tabs);
  await sleep(300);
  await u.click(".tab[aria-selected=true] .tab__close");
  await u.waitFor(`document.querySelectorAll('.tab').length === ${before.tabs}`, "关掉第二个标签", 8000);
  await u.click(".tab", 0);
  return "点回标签后直接打字进了终端";
});

await rec.step(p, "新建终端菜单：有 Git Bash，没有对勾，第一项不高亮", async () => {
  await u.click(".pl-split__more");
  await sleep(300);
  const info = await p.js(`({ items: [...document.querySelectorAll('.pl-menu [role=menuitem]')].map(x=>x.textContent.trim()), checks: [...document.querySelectorAll('.pl-menu__check')].map(x=>x.textContent).join(''), focus: document.activeElement?.getAttribute('role'), bg: getComputedStyle(document.querySelector('.pl-menu [role=menuitem]')).backgroundColor })`);
  await u.esc();
  if (!info.items.some((x) => x.startsWith("Git Bash"))) throw new Error(`没有 Git Bash：${info.items.join(" | ")}`);
  if (info.items.filter((x) => x.startsWith("Git Bash")).length !== 1) throw new Error(`Git Bash 出现不止一次：${info.items.join(" | ")}`);
  if (info.checks.includes("✓")) throw new Error("菜单里有对勾");
  if (info.focus === "menuitem") throw new Error("打开菜单时焦点落在了第一项上");
  return info.items.join(" | ");
});

await rec.step(p, "新开命令提示符很快出提示符（新版 ConPTY 启动时先问终端一句，回答不能丢，否则白等 3 秒）", async () => {
  const before = await counts(p);
  await u.click(".pl-split__more");
  await sleep(300);
  const t0 = Date.now();
  await u.clickText("Command Prompt", ".pl-menu");
  await u.waitFor(`document.querySelectorAll('.tab').length === ${before.tabs + 1}`, "多出一个标签", 10000);
  await u.waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); return /^[A-Z]:\\\\[^>]*>\\s*$/m.test(s[0]?.querySelector('.xterm-rows')?.innerText||'');})()`, "命令提示符", 15000);
  const ms = Date.now() - t0;
  // 关掉这个标签（空闲，直接关），标签数回到原样：后面的步骤按标签数写的
  await u.click(".tab[aria-selected=true] .tab__close");
  await u.waitFor(`document.querySelectorAll('.tab').length === ${before.tabs}`, "关掉命令提示符的标签", 8000);
  await u.click(".tab", 0);
  await sleep(300);
  if (ms > 2500) throw new Error(`用了 ${ms} 毫秒才出提示符`);
  return `${ms} 毫秒`;
});

// ---------- 2. 文件列表跟随本机目录 ----------
await rec.step(p, "文件列表跟随 PowerShell 的 cd", async () => {
  await u.click('.pane__btn[aria-label="Files (current directory)"]');
  await u.waitFor(`document.querySelector('.files__crumbs')?.getAttribute('data-pl-tip') || document.querySelector('.files__crumbs')?.title`, "文件列表显示目录");
  await u.click(".xterm-screen");
  await u.typeLine("cd C:\\Windows");
  const dir = await u.waitFor(`(()=>{const c=document.querySelector('.files__crumbs'); const t=c?.title||c?.getAttribute('data-pl-tip'); return /^C:\\\\Windows$/i.test(t||'') ? t : null;})()`, "列表切到 C:\\Windows");
  await u.typeLine(`cd "${HOME}"`);
  await u.waitFor(`(()=>{const c=document.querySelector('.files__crumbs'); return (c?.title||c?.getAttribute('data-pl-tip')) === ${JSON.stringify(HOME)};})()`, "列表回到家目录");
  const up = await p.js(`!!document.querySelector('.files__row--up') && !document.querySelector('.files__notice:not(.files__notice--muted)')`);
  if (!up) throw new Error("没有 .. 行，或者还有“已暂停跟随”横条");
  return `跟随到 ${dir}，再回到 ${HOME}`;
});

await rec.step(p, "文件栏路径后面的复制按钮：一点就复制当前目录的完整路径，按钮变成对勾", async () => {
  setClipboard("pl-before-copy");
  await u.click(".files__copy");
  // 对勾只亮 1.5 秒，先等对勾再读剪贴板（读剪贴板要起 PowerShell，慢）
  await u.waitFor("!!document.querySelector('.files__copy--done')", "按钮变成对勾", 10000);
  return await waitClipboard(HOME, "剪贴板里是当前目录的完整路径");
});

// ---------- 3. 文件右键菜单：新建、编辑、改名、删除 ----------
// 找到文件行并先滚动到可见区域（CI 屏幕只有 1024×768，家目录文件多时会在列表下面）
const fileRow = (name) => `(()=>{const r=[...document.querySelectorAll('.files__row')].find(r=>r.querySelector('.files__name')?.textContent===${JSON.stringify(name)}); if(!r) return null; r.scrollIntoView({block:'center'}); const b=r.getBoundingClientRect(); return {x:b.x+40,y:b.y+b.height/2};})()`;
const T1 = join(HOME, "pl-e2e.txt");
const T2 = join(HOME, "pl-e2e-renamed.txt");
for (const f of [T1, T2]) rmSync(f, { force: true });

await rec.step(p, "空白处右键新建文件，软件内编辑并保存", async () => {
  const list = await u.rect(".files__list");
  await u.clickXY(list.x + list.w / 2, list.y + list.h - 12, "right");
  await u.clickText("New file", ".pl-menu");
  await sleep(200);
  await u.type("pl-e2e.txt");
  await u.enter();
  await u.waitFor(fileRow("pl-e2e.txt"), "新文件出现在列表里");
  if (!existsSync(T1)) throw new Error("磁盘上没有这个文件");
  const pt = await p.js(fileRow("pl-e2e.txt"));
  await u.clickXY(pt.x, pt.y, "right");
  await u.clickText("Edit (in app)", ".pl-menu");
  await u.waitFor(`!!document.querySelector('.editor__text')`, "编辑器打开");
  await u.type("第一行 中文\nsecond line\n");
  await u.key("s", "KeyS", 83, 2);
  await u.waitFor(`/Saved/.test(document.querySelector('.editor__foot')?.innerText||'')`, "提示已保存");
  const content = readFileSync(T1, "utf8");
  if (!content.includes("第一行 中文") || !content.includes("second line")) throw new Error(`磁盘内容不对：${JSON.stringify(content)}`);
  await u.esc();
  await u.waitFor(`!document.querySelector('.editor')`, "编辑器关闭");
  return JSON.stringify(content);
});

await rec.step(p, "F2 改名、改成已有名字时报错", async () => {
  const pt = await p.js(fileRow("pl-e2e.txt"));
  if (!pt) throw new Error("找不到上一步的文件");
  await u.clickXY(pt.x, pt.y);
  await u.key("F2", "F2", 113);
  await sleep(200);
  await u.key("a", "KeyA", 65, 2);
  await u.type("pl-e2e-renamed.txt");
  await u.enter();
  await u.waitFor(fileRow("pl-e2e-renamed.txt"), "改名后的文件出现");
  if (!existsSync(T2) || existsSync(T1)) throw new Error("磁盘上的改名不对");
  // 只改大小写
  await u.key("F2", "F2", 113);
  await sleep(200);
  await u.key("a", "KeyA", 65, 2);
  await u.type("PL-E2E-RENAMED.txt");
  await u.enter();
  await u.waitFor(fileRow("PL-E2E-RENAMED.txt"), "只改大小写成功");
});

await rec.step(p, "Delete 删除进回收站", async () => {
  const pt = await p.js(fileRow("PL-E2E-RENAMED.txt"));
  if (!pt) throw new Error("找不到上一步的文件");
  await u.clickXY(pt.x, pt.y);
  await u.key("Delete", "Delete", 46);
  await u.waitFor(`/Recycle Bin/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "回收站确认框");
  await u.clickText("Move to Recycle Bin", ".pl-dialog");
  await u.waitFor(`!${fileRow("PL-E2E-RENAMED.txt")}`, "文件从列表消失");
  if (existsSync(T2)) throw new Error("磁盘上还在");
  // 回收站里能找到
  const found = execFileSync("powershell.exe", ["-NoProfile", "-Command", "(New-Object -ComObject Shell.Application).NameSpace(10).Items() | Where-Object { $_.Name -like 'PL-E2E-RENAMED*' } | Measure-Object | Select-Object -ExpandProperty Count"]).toString().trim();
  if (found === "0") throw new Error("回收站里没找到");
  return `回收站里找到 ${found} 个`;
});

await rec.step(p, "图片等二进制文件右键没有“编辑”", async () => {
  const png = join(HOME, "pl-e2e.png");
  writeFileSync(png, Buffer.from("89504e470d0a1a0a0000000d4948445200", "hex"));
  await u.click(".files__tools .pane__btn[aria-label='Refresh']").catch(() => {});
  await u.key("F5", "F5", 116);
  await u.waitFor(fileRow("pl-e2e.png"), "png 出现在列表里");
  const pt = await p.js(fileRow("pl-e2e.png"));
  await u.clickXY(pt.x, pt.y, "right");
  const items = await p.js(`[...document.querySelectorAll('.pl-menu [role=menuitem]')].map(x=>x.textContent.trim()).join(' | ')`);
  await u.esc();
  rmSync(png, { force: true });
  if (items.includes("Edit (in app)")) throw new Error(`菜单里有编辑：${items}`);
  return items;
});

// ---------- 4. 响铃提醒、关闭规则、粘贴 ----------
await rec.step(p, "程序响铃：看不见的标签上出现提醒点，当前标签没有；切过去就消失", async () => {
  // 第一个标签里 3 秒后响铃，期间开第二个标签（第一个就看不见了）
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.typeLine('Start-Sleep 3; Write-Host "PL-BELL`a"');
  await u.click('.tabbar button[aria-label="New terminal tab"]');
  await u.waitFor(`document.querySelectorAll('.tab').length === 2`, "第二个标签");
  await u.waitFor(`!!document.querySelectorAll('.tab')[0]?.querySelector('.tab__attention')`, "第一个标签上的提醒点", 20000);
  if (await p.js(`!!document.querySelector('.tab[aria-selected=true] .tab__attention')`)) throw new Error("当前标签也出现了提醒点");
  await u.click(".tab", 0);
  await u.waitFor(`!document.querySelector('.tab__attention')`, "切过去后提醒点消失", 8000);
  // 关掉第二个标签（新开的空闲终端）
  await u.click(".tab", 1);
  await sleep(500);
  await u.click(".tab[aria-selected=true] .tab__close");
  await u.waitFor(`document.querySelectorAll('.tab').length === 1`, "关掉第二个标签", 8000);
  return "第一个标签出现提醒点，切过去后消失";
});

await rec.step(p, "空闲终端直接关闭；运行 ping 时关闭要确认", async () => {
  await u.click('.tabbar button[aria-label="New terminal tab"]');
  await u.waitFor(`document.querySelectorAll('.tab').length === 2`, "第二个标签");
  await u.waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); return /PS |>/.test(s[0]?.querySelector('.xterm-rows')?.innerText||'');})()`, "新终端出现提示符", 30000);
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.typeLine("ping -t 127.0.0.1");
  await sleep(2000);
  await u.click(".tab[aria-selected=true] .tab__close");
  await u.waitFor(`/still running/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "运行中的确认框", 8000);
  await u.clickText("Cancel", ".pl-dialog");
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.key("c", "KeyC", 67, 2);
  await sleep(1500);
  await u.click(".tab[aria-selected=true] .tab__close");
  await sleep(2500);
  const d = await u.dialogText();
  if (d) throw new Error(`空闲时还弹了确认框：${d}`);
  if ((await counts(p)).tabs !== 1) throw new Error("标签没关掉");
});

await rec.step(p, "Ctrl+Shift+V 只粘贴一次", async () => {
  setClipboard("PLPASTEONCE");
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.key("V", "KeyV", 86, 10);
  await sleep(1200);
  const n = ((await u.termText()) ?? "").match(/PLPASTEONCE/g)?.length ?? 0;
  // 清掉这行，不执行
  await u.key("Escape", "Escape", 27);
  if (n !== 1) throw new Error(`粘贴了 ${n} 次`);
});

await rec.step(p, "PowerShell 里 Shift+Enter 换行不执行，回车后两行都执行", async () => {
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.type("echo PLSE1");
  await u.key("Enter", "Enter", 13, 8, "\r");
  await u.type("echo PLSE2");
  await sleep(800);
  const before = (await u.termText()) ?? "";
  if (/^PLSE1\s*$/m.test(before)) throw new Error(`Shift+Enter 直接执行了：${before.slice(-300)}`);
  await u.enter();
  await u.waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]?.querySelector('.xterm-rows')?.innerText||''; return /^PLSE1\s*$/m.test(s) && /^PLSE2\s*$/m.test(s);})()`, "两行命令都执行了", 10000);
});

/** 和 Codex 一样读 Windows 控制台原生按键的全屏程序：进备用屏读一个键，回主屏打印 PL-KEY=键+修饰键；最多等 15 秒 */
const READKEY_PS1 = join(OUT, "pl-readkey.ps1");
writeFileSync(READKEY_PS1, [
  'Write-Host -NoNewline "$([char]27)[?1049h"',
  "$sw = [Diagnostics.Stopwatch]::StartNew()",
  "while (-not [Console]::KeyAvailable -and $sw.Elapsed.TotalSeconds -lt 15) { Start-Sleep -Milliseconds 50 }",
  "$k = if ([Console]::KeyAvailable) { [Console]::ReadKey($true) }",
  'Write-Host -NoNewline "$([char]27)[?1049l"',
  "\"PL-KEY=$(if ($k) { \"$($k.Key)+$($k.Modifiers)\" } else { 'none' })\"",
  "",
].join("\r\n"));
const visibleRows = `(document.querySelector('.pane-slot:not([style*="none"]) .xterm-rows')?.innerText||'')`;
/** 在当前终端里跑 command（启动读键程序），等进了备用屏按 Shift+Enter，返回程序读到的键 */
async function shiftEnterInReadKey(command) {
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.typeLine(command);
  await u.waitFor(`${visibleRows}.trim() === ''`, "进了备用屏", 15000);
  await u.key("Enter", "Enter", 13, 8, "\r");
  return u.waitFor(`(()=>{const m=${visibleRows}.match(/^PL-KEY=(\\S+)\\s*$/m); return m && m[1];})()`, "程序读到的按键", 20000);
}

await rec.step(p, "本机 PowerShell 里的全屏程序（像本机的 Codex）：Shift+Enter 原样收到 Shift+Enter（和 Windows Terminal 一样），不会变成 Ctrl+Enter", async () => {
  const got = await shiftEnterInReadKey(`& "${READKEY_PS1}"`);
  if (got !== "Enter+Shift") throw new Error(`程序读到的是 ${got}，应为 Enter+Shift`);
  return got;
});

await rec.step(p, "本机 Git Bash 里开的 Windows 全屏程序（像在 Git Bash 里开 Codex）：Shift+Enter 也原样收到", async () => {
  const before = await counts(p);
  await u.click(".pl-split__more");
  await sleep(300);
  await u.clickText("Git Bash", ".pl-menu");
  await u.waitFor(`document.querySelectorAll('.tab').length === ${before.tabs + 1}`, "多出一个标签", 10000);
  await u.waitFor(`/\\$ *$/m.test(${visibleRows})`, "Git Bash 提示符", 30000);
  try {
    const got = await shiftEnterInReadKey(`powershell.exe -NoProfile -ExecutionPolicy Bypass -File '${READKEY_PS1}'`);
    if (got !== "Enter+Shift") throw new Error(`程序读到的是 ${got}，应为 Enter+Shift`);
    return got;
  } finally {
    await sleep(800);
    await closeCurrentTab(before.tabs);
    await u.click(".tab", 0);
  }
});

await rec.step(p, "Ctrl+V 也只粘贴一次", async () => {
  setClipboard("PLCTRLV");
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.key("v", "KeyV", 86, 2);
  await sleep(1200);
  const n = ((await u.termText()) ?? "").match(/PLCTRLV/g)?.length ?? 0;
  await u.key("Escape", "Escape", 27);
  if (n !== 1) throw new Error(`粘贴了 ${n} 次`);
});

await rec.step(p, "用过 Shift+Enter 之后，Esc 清掉输入行，Ctrl+C 取消输入行", async () => {
  // Shift+Enter 发的是 Windows 控制台的按键格式。控制台收到过这种格式后，曾把单独的 Esc 当成没收全的序列扣住，
  // 接着的按键和它拼在一起（Ctrl+C 变成文字 ^C 收进输入行）
  const lineAlone = (word) => `(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]?.querySelector('.xterm-rows')?.innerText||''; return new RegExp('^${word}\\\\s*$','m').test(s);})()`;
  await u.click(".pane-slot:not([style*='none']) .xterm-screen");
  await u.type("PLESCME");
  await sleep(500);
  await u.esc();
  await sleep(500);
  await u.typeLine('echo "PLAFTERESC"');
  await u.waitFor(lineAlone("PLAFTERESC"), "Esc 清掉了输入行，下一条命令照常执行", 10000);
  await u.type("PLCANCELME");
  await sleep(500);
  await u.esc();
  await sleep(300);
  await u.key("c", "KeyC", 67, 2);
  await sleep(800);
  await u.typeLine('echo "PLAFTERCTRLC"');
  await u.waitFor(lineAlone("PLAFTERCTRLC"), "Esc、Ctrl+C 之后下一条命令照常执行", 10000);
});

// ---------- 4b. 0.9.2：复制、右键菜单、程序写剪贴板、链接 ----------
await rec.step(p, "选中文字后 Ctrl+C 复制；Ctrl+Shift+C 也能复制", async () => {
  await freshPrompt();
  // 加引号：PowerShell 的 echo 会把不带引号的几个词分成几行输出
  await u.typeLine('echo "PLCOPYA PLCOPYB"');
  await u.waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]?.querySelector('.xterm-rows')?.innerText||''; return /^PLCOPYA PLCOPYB\\s*$/m.test(s);})()`, "echo 的输出", 10000);
  setClipboard("empty");
  let at = await wordAt("PLCOPYA");
  await u.clickXY(at.x, at.y, "left", 2);
  await u.key("c", "KeyC", 67, 2);
  await waitClipboard("PLCOPYA", "Ctrl+C 复制");
  at = await wordAt("PLCOPYB");
  await u.clickXY(at.x, at.y, "left", 2);
  await u.key("C", "KeyC", 67, 10);
  await waitClipboard("PLCOPYB", "Ctrl+Shift+C 复制");
  return "Ctrl+C、Ctrl+Shift+C 都复制了";
});

await rec.step(p, "右键菜单：有复制当前目录、在资源管理器中打开、左右和上下分屏，没有全选和清屏", async () => {
  const term = await u.rect(".pane-slot:not([style*='none']) .xterm-screen");
  const openMenu = async () => {
    await u.clickXY(term.x + term.w / 2, term.y + term.h / 2, "right");
    return u.waitFor(`(()=>{const m=document.querySelector('[role=menu]'); return m ? [...m.querySelectorAll('[role=menuitem]')].map(x=>x.textContent.trim()) : null;})()`, "终端的右键菜单", 5000);
  };
  const items = await openMenu();
  for (const want of ["Copy", "Paste", "Copy current folder", "Open in File Explorer", "Split left and right", "Split top and bottom"]) {
    if (!items.some((x) => x.startsWith(want))) throw new Error(`菜单里没有“${want}”：${items.join(" / ")}`);
  }
  if (items.some((x) => /^(Select all|Clear)/.test(x))) throw new Error(`菜单里还有全选或清屏：${items.join(" / ")}`);
  setClipboard("empty");
  await u.clickText("Copy current folder", "[role=menu]");
  await sleep(800);
  const dir = getClipboard();
  if (!/^[A-Za-z]:\\/.test(dir) || !existsSync(dir)) throw new Error(`复制的当前目录不对：${dir}`);
  const before = (await counts(p)).panes;
  await openMenu();
  await u.clickText("Split left and right", "[role=menu]");
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').length === ${before + 1}`, "旁边多了一个窗格", 10000);
  await u.waitFor(`/PS /.test([...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[1]?.querySelector('.xterm-rows')?.innerText||'')`, "新窗格出现提示符", 30000);
  await sleep(1500);
  await u.click(".pane-slot:not([style*='none']) .pane__btn--close", 1);
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').length === ${before}`, "新窗格关掉了", 10000);
  return `菜单：${items.join(" / ")}｜当前目录 ${dir}`;
});

await rec.step(p, "程序发来的复制请求（OSC 52）写进 Windows 剪贴板，中文不乱码", async () => {
  setClipboard("empty");
  await freshPrompt();
  await u.typeLine("[Console]::Write([char]27 + ']52;c;' + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes('PLOSC52 中文')) + [char]7)");
  await waitClipboard("PLOSC52 中文", "OSC 52 复制");
  return "剪贴板是 PLOSC52 中文";
});

await rec.step(p, "链接：拖选链接里的文字不弹确认框；单击弹出确认框，能复制链接、用系统浏览器打开", async () => {
  const url = "https://example.com/pierlink-e2e-link";
  await freshPrompt();
  await u.typeLine(`echo ${url}`);
  await u.waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]?.querySelector('.xterm-rows')?.innerText||''; return s.split(/\\n/).some(l=>l.trim()===${JSON.stringify(url)});})()`, "echo 的输出", 10000);
  const at = await wordAt(url);
  await u.drag(at.left + 40, at.y, at.right - 40, at.y);
  await sleep(1200);
  const afterDrag = await u.dialogText();
  if (afterDrag) throw new Error(`拖选链接里的文字弹出了确认框：${afterDrag}`);
  const openDialog = async () => {
    // 先移到旁边的格子再移回来：xterm 只在鼠标换到新的格子时才重新识别链接，回到原来那一格不算
    await u.mouse("mouseMoved", at.x - 20, at.y, "none", 0);
    await sleep(200);
    await u.mouse("mouseMoved", at.x, at.y, "none", 0);
    await sleep(600);
    await u.clickXY(at.x, at.y);
    return u.waitFor(`document.querySelector('.pl-dialog')?.innerText || null`, "链接确认框", 5000);
  };
  const d = await openDialog();
  if (!d.includes(url)) throw new Error(`确认框里没有这个链接：${d}`);
  setClipboard("empty");
  await u.clickText("Copy link", ".pl-dialog");
  await waitClipboard(url, "复制链接");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "复制后确认框关掉", 5000);
  const edges = edgeCount();
  await openDialog();
  await u.clickText("Open", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "打开后确认框关掉（没有报错）", 8000);
  const deadline = Date.now() + 20000;
  while (edgeCount() <= edges && Date.now() < deadline) await sleep(1000);
  const opened = edgeCount() > edges;
  try { execFileSync("taskkill.exe", ["/IM", "msedge.exe", "/F"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
  if (!opened) throw new Error("点“打开”后没有启动系统浏览器（Edge 进程没有出现）");
  return "拖选不弹框；复制链接、用 Edge 打开都正常";
});

await rec.step(p, "打开“右键复制粘贴”：有选中时右键复制、没选中时右键粘贴，都不弹菜单", async () => {
  const toggle = async (on) => {
    await u.clickText("Settings", "nav");
    await u.clickText("Terminal", ".settings__nav");
    const sel = `[role=switch][aria-label="Right-click copy and paste"]`;
    await u.waitFor(`!!document.querySelector(${JSON.stringify(sel)})`, "设置里的“右键复制粘贴”");
    if ((await p.js(`document.querySelector(${JSON.stringify(sel)}).getAttribute('aria-checked')`)) !== String(on)) await u.click(sel);
    await u.waitFor(`document.querySelector(${JSON.stringify(sel)})?.getAttribute('aria-checked')===${JSON.stringify(String(on))}`, "开关切换好");
    await u.clickText("Workspace", "nav");
    await sleep(600);
  };
  await toggle(true);
  try {
    await freshPrompt();
    await u.typeLine("echo PLRIGHTCOPY");
    await u.waitFor(`/^PLRIGHTCOPY\\s*$/m.test([...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]?.querySelector('.xterm-rows')?.innerText||'')`, "echo 的输出", 10000);
    setClipboard("empty");
    const at = await wordAt("PLRIGHTCOPY");
    await u.clickXY(at.x, at.y, "left", 2);
    await u.clickXY(at.x, at.y, "right");
    await waitClipboard("PLRIGHTCOPY", "右键复制");
    if (await p.js(`!!document.querySelector('[role=menu]')`)) throw new Error("右键复制后还弹出了菜单");
    setClipboard("PLRIGHTPASTE");
    const term = await u.rect(".pane-slot:not([style*='none']) .xterm-screen");
    await u.clickXY(term.x + term.w / 2, term.y + term.h - 12, "right");
    await sleep(1200);
    if (await p.js(`!!document.querySelector('[role=menu]')`)) throw new Error("没选中时右键弹出了菜单");
    const n = ((await u.termText()) ?? "").match(/PLRIGHTPASTE/g)?.length ?? 0;
    await u.key("Escape", "Escape", 27);
    if (n !== 1) throw new Error(`右键粘贴了 ${n} 次`);
  } finally {
    await toggle(false);
  }
  return "右键复制、右键粘贴各一次，没有菜单";
});

await rec.step(p, "设置里关掉“PowerShell 提示音”：新开的 PowerShell 不响（PSReadLine 的 BellStyle 为 None）；打开后照旧", async () => {
  const sel = `[role=switch][aria-label="PowerShell beep"]`;
  const toggle = async (on) => {
    await u.clickText("Settings", "nav");
    await u.clickText("Terminal", ".settings__nav");
    await u.waitFor(`!!document.querySelector(${JSON.stringify(sel)})`, "设置里的“PowerShell 提示音”");
    if ((await p.js(`document.querySelector(${JSON.stringify(sel)}).getAttribute('aria-checked')`)) !== String(on)) await u.click(sel);
    await u.waitFor(`document.querySelector(${JSON.stringify(sel)})?.getAttribute('aria-checked')===${JSON.stringify(String(on))}`, "开关切换好");
    await u.clickText("Workspace", "nav");
    await sleep(600);
  };
  const term = `([...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0]?.querySelector('.xterm-rows')?.innerText||'')`;
  // 设置只对新开的终端生效：退出 PowerShell，按 Enter 重新打开，再读 PSReadLine 的 BellStyle
  const bellStyle = async (what) => {
    await freshPrompt();
    await u.typeLine("exit");
    await u.waitFor(`!!document.querySelector('.pane-slot:not([style*="none"]) .term-endcard')`, `${what}：PowerShell 退出`, 15000);
    await u.enter();
    await u.waitFor(`!document.querySelector('.pane-slot:not([style*="none"]) .term-endcard') && /PS .+>\\s*$/.test(${term})`, `${what}：重新打开的 PowerShell 提示符`, 30000);
    await u.typeLine(`"PL-BELL=" + (Get-PSReadLineOption).BellStyle`);
    return await u.waitFor(`(()=>{const m=${term}.match(/^PL-BELL=(\\w+)\\s*$/m); return m && m[1];})()`, `${what}：读到 BellStyle`, 15000);
  };
  await toggle(false);
  let off;
  try {
    off = await bellStyle("关掉提示音后");
  } finally {
    await toggle(true);
  }
  if (off !== "None") throw new Error(`关掉提示音后 BellStyle 是 ${off}，应为 None`);
  const on = await bellStyle("打开提示音后");
  if (on === "None") throw new Error("打开提示音后 BellStyle 还是 None");
  return `关：${off}；开：${on}`;
});

// ---------- 5. 分屏与拖动 ----------
await rec.step(p, "分屏后拖动窗格换位置，终端不重建", async () => {
  // 只在当前标签里操作：先切到第一个标签，标记它可见的第一个终端
  await u.click(".tab", 0);
  await sleep(300);
  await p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[0].querySelector('.term-pane').dataset.mark='A'`);
  await u.click('.pane__btn[aria-label="Split side by side"], .tabbar button[aria-label="Split right"]');
  await sleep(300);
  await u.clickText("Another", ".pl-menu");
  await u.waitFor(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').length===2`, "两个窗格");
  const a = await p.js(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].find(s=>s.querySelector('.term-pane')?.dataset.mark==='A'); const t=s.querySelector('.pane__title').getBoundingClientRect(); const r=s.getBoundingClientRect(); return {tx:t.x+20,ty:t.y+8,x:r.x}})()`);
  const other = await p.js(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').find(s=>s.querySelector('.term-pane')?.dataset.mark!=='A'); const r=s.getBoundingClientRect(); return {x:r.x,y:r.y,w:r.width,h:r.height}})()`);
  await u.drag(a.tx, a.ty, other.x + other.w / 2, other.y + other.h / 2);
  const after = await p.js(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].find(s=>s.querySelector('.term-pane')?.dataset.mark==='A'); return s ? s.getBoundingClientRect().x : null})()`);
  if (after === null) throw new Error("A 终端被重建了");
  if (Math.abs(after - a.x) < 5) throw new Error("位置没变");
  if (!(await u.termText(0)) && !(await u.termText(1))) throw new Error("终端内容丢了");
  return `A 从 x=${Math.round(a.x)} 换到 x=${Math.round(after)}`;
});

// ---------- 6. 服务器：本机 sshd ----------
const haveSshd = !!(KEY && USER);
const remoteDir = join(HOME, "pl-remote");
await rec.step(p, "添加服务器（本机 sshd，私钥登录）并连接", async () => {
  if (!haveSshd) throw new Error("跳过：没有本机 sshd 环境变量");
  await u.click('[aria-label="Add server"]');
  await u.waitFor(`!!document.querySelector('.pl-dialog')`, "添加服务器对话框");
  const fill = async (label, value) => {
    const ok = await p.js(`(()=>{const l=[...document.querySelectorAll('.pl-dialog label.pl-field')].find(l=>l.textContent.trim().startsWith(${JSON.stringify(label)})); const i=l?.querySelector('input'); if(!i) return false; i.focus(); i.select(); return true;})()`);
    if (!ok) throw new Error(`找不到输入框：${label}`);
    await u.type(value);
  };
  await fill("Host", "127.0.0.1");
  await fill("User", USER);
  await u.clickText("Specific key", ".pl-dialog");
  // 1.2.12 起私钥要先导入（加密存进 Pierlink），再从列表里选；导入过的同一把直接用原来那一把
  await u.clickText("Import private key", ".pl-dialog");
  await u.waitFor(`!!document.querySelector('.key-import__file')`, "导入私钥对话框");
  await fill("Key file", KEY);
  await u.clickText("Import", ".pl-dialog:has(.key-import__file)");
  await u.waitFor(`!document.querySelector('.key-import__file')`, "私钥导入好", 15000);
  await u.clickText("Save", ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
  // 列表里可能已有升级测试留下的服务器，按地址找这一行
  const row = await u.waitFor(`(()=>{const i=[...document.querySelectorAll('.srv-row')].findIndex(r=>r.innerText.includes('127.0.0.1')); return i>=0 ? String(i) : null;})()`, "服务器出现在列表里");
  await u.click(".srv-row", Number(row), "left", 2);
  await u.waitFor(`/Trust and connect/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "主机指纹确认", 20000);
  await u.clickText("Trust and connect", ".pl-dialog");
  // 只认服务器那个标签里的终端（本机 PowerShell 的提示符也以 > 结尾，不能混淆）
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "连接对话框关闭（指纹确认后不应一直“正在连接”）", 30000);
  await u.waitFor(`(()=>{const t=document.querySelector('.tab[aria-selected=true]'); if(!t || !t.innerText.includes('127.0.0.1')) return false; const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); return s.some(x=>/runneradmin@|C:\\\\Users\\\\[^>]*>/i.test(x.querySelector('.xterm-rows')?.innerText||''));})()`, "服务器终端出现提示符", 30000);
});

await rec.step(p, "拖放传输：本机 → 服务器，再服务器 → 本机（同名提示里没有 \\\\?\\）", async () => {
  if (!haveSshd) throw new Error("跳过：没有本机 sshd");
  mkdirSync(remoteDir, { recursive: true });

  rmSync(join(remoteDir, "pl-xfer.txt"), { force: true });
  // 把服务器标签拖进一个本机标签，成为左右分屏
  const tabs = await p.js(`[...document.querySelectorAll('.tab')].map(t=>{const r=t.getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2,text:t.innerText}})`);
  const server = tabs.find((t) => t.text.includes("127.0.0.1"));
  const localTab = tabs.find((t) => !t.text.includes("127.0.0.1"));
  if (!server || !localTab) throw new Error(`标签不对：${JSON.stringify(tabs.map((t) => t.text))}`);
  await u.clickXY(localTab.x, localTab.y);
  await sleep(400);
  const pane = await p.js(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); const r=s[s.length-1].getBoundingClientRect(); return {x:r.x,y:r.y,w:r.width,h:r.height}})()`);
  // 放到下半边：本机标签已分成左右两格，再放右边会小于最小宽度（应用会拒绝，这是对的）
  await u.drag(server.x, server.y, pane.x + pane.w / 2, pane.y + pane.h - 40);
  await u.waitFor(`document.querySelectorAll('.tab').length===${tabs.length - 1} && [...document.querySelectorAll('.tab')].some(t=>/2 panes|3 panes/.test(t.innerText))`, "服务器标签并进本机标签");
  // 打开所有窗格的文件列表
  const n = await p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none').length`);
  for (let i = 0; i < n; i++) {
    const open = await p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[${i}].querySelector('.pane__btn[aria-label="Files (current directory)"]')?.getAttribute('aria-pressed')`);
    if (open !== "true") await p.js(`[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')[${i}].querySelector('.pane__btn[aria-label="Files (current directory)"]').click()`);
  }
  await sleep(2500);
  // 找服务器那个文件列表（路径以 / 开头），进入 pl-remote
  const panels = () => p.js(`[...document.querySelectorAll('.files')].map((f,i)=>({i, path: f.querySelector('.files__crumbs')?.title || f.querySelector('.files__crumbs')?.getAttribute('data-pl-tip') || ''}))`);
  const ps = await u.waitFor(`(()=>{const l=[...document.querySelectorAll('.files')].map((f,i)=>({i, path: f.querySelector('.files__crumbs')?.title || f.querySelector('.files__crumbs')?.getAttribute('data-pl-tip') || ''})); return l.some(x=>x.path.startsWith('/')) && l.some(x=>/^[A-Z]:\\\\/i.test(x.path)) ? l : null})()`, "本机和服务器两个文件列表都读出来了", 20000);
  const remote = ps.find((x) => x.path.startsWith("/")).i;
  const local = ps.find((x) => /^[A-Z]:\\/i.test(x.path)).i;
  const rowIn = (panel, name) => `(()=>{const r=[...document.querySelectorAll('.files')[${panel}].querySelectorAll('.files__row')].find(r=>r.querySelector('.files__name')?.textContent===${JSON.stringify(name)}); if(!r) return null; r.scrollIntoView({block:'center'}); const b=r.getBoundingClientRect(); return {x:b.x+40,y:b.y+b.height/2};})()`;
  const pr = await u.waitFor(rowIn(remote, "pl-remote"), "服务器列表里有 pl-remote");
  await u.clickXY(pr.x, pr.y, "left", 2);
  await u.waitFor(`/pl-remote$/.test(document.querySelectorAll('.files')[${remote}].querySelector('.files__crumbs')?.title || document.querySelectorAll('.files')[${remote}].querySelector('.files__crumbs')?.getAttribute('data-pl-tip') || '')`, "服务器列表进入 pl-remote");
  // 源文件放在本机文件列表当前所在的目录
  const localDir = ps.find((x) => x.i === local).path;
  const src = join(localDir, "pl-xfer.txt");
  writeFileSync(src, "transfer test 传输测试\n");
  await p.js(`document.querySelectorAll('.files')[${local}].querySelector('.files__list').focus()`);
  await u.key("F5", "F5", 116);
  const lr = await u.waitFor(rowIn(local, "pl-xfer.txt"), "本机列表里有 pl-xfer.txt");
  const target = await p.js(`(()=>{const r=document.querySelectorAll('.files')[${remote}].getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}})()`);
  await u.drag(lr.x, lr.y, target.x, target.y);
  await u.waitFor(`!document.querySelector('.pl-dialog')`, "没有确认框（不应同名）", 5000);
  await u.waitFor(rowIn(remote, "pl-xfer.txt"), "服务器列表出现 pl-xfer.txt", 30000);
  if (readFileSync(join(remoteDir, "pl-xfer.txt"), "utf8") !== "transfer test 传输测试\n") throw new Error("传过去的内容不对");
  const xferBtn = await p.js(`document.querySelector('.xfer-btn')?.innerText`);
  // 反方向：服务器 → 本机，本机已有同名文件
  const rr = await p.js(rowIn(remote, "pl-xfer.txt"));
  const ltarget = await p.js(`(()=>{const r=document.querySelectorAll('.files')[${local}].getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}})()`);
  await u.drag(rr.x, rr.y, ltarget.x, ltarget.y);
  const dlg = await u.waitFor(`document.querySelector('.pl-dialog')?.innerText`, "同名确认框", 15000);
  if (dlg.includes("\\\\?\\")) throw new Error(`路径里有 \\\\?\\：${dlg}`);
  await u.clickText("Skip existing", ".pl-dialog");
  return `传输按钮：${xferBtn}；同名提示：${dlg.replace(/\s+/g, " ").slice(0, 160)}`;
});

// 选文件夹是系统对话框，端到端里不好点：这里只查菜单项在不在；拖到资源管理器也模拟不了
await rec.step(p, "服务器文件右键有“下载到…”，本机文件没有", async () => {
  if (!haveSshd) throw new Error("跳过：没有本机 sshd");
  const paths = await p.js(`[...document.querySelectorAll('.files')].map(f=>f.querySelector('.files__crumbs')?.title || f.querySelector('.files__crumbs')?.getAttribute('data-pl-tip') || '')`);
  const remote = paths.findIndex((x) => x.startsWith("/"));
  const local = paths.findIndex((x) => /^[A-Z]:\\/i.test(x));
  if (remote < 0 || local < 0) throw new Error(`文件列表不对：${JSON.stringify(paths)}`);
  const menuOf = async (panel) => {
    const pt = await u.waitFor(`(()=>{const r=[...document.querySelectorAll('.files')[${panel}].querySelectorAll('.files__row')].find(r=>r.querySelector('.files__name')?.textContent==='pl-xfer.txt'); if(!r) return null; r.scrollIntoView({block:'center'}); const b=r.getBoundingClientRect(); return {x:b.x+40,y:b.y+b.height/2};})()`, "列表里有 pl-xfer.txt");
    await u.clickXY(pt.x, pt.y, "right");
    const items = await u.waitFor(`(()=>{const l=[...document.querySelectorAll('.pl-menu [role=menuitem]')].map(x=>x.textContent.trim()); return l.length ? l : null;})()`, "右键菜单");
    await u.esc();
    return items;
  };
  const remoteItems = await menuOf(remote);
  if (!remoteItems.some((x) => x.startsWith("Download to"))) throw new Error(`服务器文件的菜单里没有 Download to…：${remoteItems.join(" | ")}`);
  const localItems = await menuOf(local);
  if (localItems.some((x) => x.startsWith("Download to"))) throw new Error(`本机文件的菜单里有 Download to…：${localItems.join(" | ")}`);
  return remoteItems.join(" | ");
});

// ---------- 7. 重启恢复 ----------
await rec.step(() => p, "强制结束后重新打开：布局恢复，服务器窗格按 Enter 重连", async () => {
  const before = await counts(p);
  await sleep(1000);
  p.close();
  kill();
  await sleep(2000);
  ({ p, u } = await launch());
  await sleep(2000);
  const after = await counts(p);
  if (after.tabs !== before.tabs || after.panes !== before.panes) throw new Error(`布局不一致：之前 ${JSON.stringify(before)}，之后 ${JSON.stringify(after)}`);
  if (haveSshd) {
    // 服务器窗格可能在另一个标签里：先切到它所在的标签
    const tab = await p.js(`(()=>{const t=[...document.querySelectorAll('.tab')].find(t=>t.innerText.includes('127.0.0.1')); if(!t) return null; const r=t.getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}})()`);
    if (tab) await u.clickXY(tab.x, tab.y);
    await u.waitFor(`[...document.querySelectorAll('.term-ended--restored')].some(e=>e.getBoundingClientRect().width>0)`, "服务器窗格显示未连接");
    const restored = await p.js(`(()=>{const e=[...document.querySelectorAll('.term-ended--restored')].find(e=>e.getBoundingClientRect().width>0); const r=e.getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+40}})()`);
    await u.clickXY(restored.x, restored.y);
    await u.enter();
    await u.waitFor(`![...document.querySelectorAll('.term-ended--restored')].some(e=>e.getBoundingClientRect().width>0)`, "按 Enter 后开始连接", 20000);
  }
  return `之前 ${JSON.stringify(before)}，之后 ${JSON.stringify(after)}`;
});

// ---------- 8. 资源管理器右键菜单“在 Pierlink 中打开” ----------
const MENU_KEYS = ["Directory\\Background\\shell\\Pierlink", "Directory\\shell\\Pierlink", "Drive\\shell\\Pierlink"];
/** 读一处右键菜单项（当前用户的注册表）：文字、图标、命令；没有这一项时返回 null */
function menuEntry(key) {
  const out = pwsh(`$k='HKCU:\\Software\\Classes\\${key}'; if (-not (Test-Path -LiteralPath $k)) { 'null' } else { $a=Get-ItemProperty -LiteralPath $k; $c=Get-ItemProperty -LiteralPath ($k + '\\command') -ErrorAction SilentlyContinue; [pscustomobject]@{label=$a.'(default)'; icon=$a.Icon; command=$c.'(default)'} | ConvertTo-Json -Compress }`);
  return JSON.parse(out);
}
/** 等三处菜单项都注册（want=true）或都删掉（want=false） */
async function menuState(want) {
  for (let i = 0; i < 40; i++) {
    const all = MENU_KEYS.map(menuEntry);
    if (all.every((e) => !!e === want)) return all;
    await sleep(500);
  }
  throw new Error(`右键菜单项${want ? "没有都注册" : "没有都删掉"}：${JSON.stringify(MENU_KEYS.map(menuEntry))}`);
}
/** 像资源管理器那样执行菜单命令：把 %V 换成目录，命令行原样交给 CreateProcess（不再加引号、不转义） */
function runMenuCommand(command, dir, env = process.env) {
  const m = command.replace("%V", dir).match(/^"([^"]+)" (.*)$/);
  if (!m) throw new Error(`菜单命令格式不对：${command}`);
  const child = spawn(m[1], [m[2]], { argv0: `"${m[1]}"`, windowsVerbatimArguments: true, detached: true, stdio: "ignore", env });
  child.unref();
}
/** 当前显示的终端里 PowerShell 提示符中的目录 */
const visiblePrompt = `(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); const m=(s[0]?.querySelector('.xterm-rows')?.innerText||'').match(/PS (.+?)>/); return m && m[1];})()`;
/** 执行菜单命令后：多出一个标签，里面的终端就在 dir */
async function openedAt(cmd, dir, want = dir) {
  const before = await counts(p);
  runMenuCommand(cmd, dir);
  await u.waitFor(`document.querySelectorAll('.tab').length === ${before.tabs + 1}`, "多出一个标签", 20000);
  const prompt = await u.waitFor(visiblePrompt, "新标签里的 PowerShell 提示符", 30000);
  if (prompt.toLowerCase() !== want.toLowerCase()) throw new Error(`起始目录是 ${prompt}，应为 ${want}`);
  await closeCurrentTab(before.tabs);
  return prompt;
}
/**
 * 关掉当前标签，等标签数回到 tabs。刚跑完命令时 Git Bash 还在算提示符（里面要跑 git），
 * 这时关会先问一句“还有程序在运行”：照常点“关闭标签”
 */
async function closeCurrentTab(tabs) {
  await u.click(".tab[aria-selected=true] .tab__close");
  for (let i = 0; i < 25; i++) {
    if (await p.js(`document.querySelectorAll('.tab').length === ${tabs}`)) return;
    if (await p.js(`!!document.querySelector('.pl-dialog')`)) await u.clickText("Close tab", ".pl-dialog");
    await sleep(400);
  }
  throw new Error(`刚开的标签没关掉：${(await u.dialogText()) || "没有对话框"}`);
}
const MENU_DIR = join(HOME, "pl e2e 右键 目录");
mkdirSync(MENU_DIR, { recursive: true });

await rec.step(p, "资源管理器右键菜单：文件夹、空白处、磁盘三处都有“Open in Pierlink”，图标和命令都指向装好的 Pierlink.exe", async () => {
  const all = await menuState(true);
  const want = `"${EXE}" --open-dir "%V"`;
  all.forEach((e, i) => {
    if (e.label !== "Open in Pierlink") throw new Error(`${MENU_KEYS[i]} 的文字是 ${e.label}`);
    if (e.command?.toLowerCase() !== want.toLowerCase()) throw new Error(`${MENU_KEYS[i]} 的命令是 ${e.command}，应为 ${want}`);
    if (e.icon?.toLowerCase() !== EXE.toLowerCase()) throw new Error(`${MENU_KEYS[i]} 的图标是 ${e.icon}`);
  });
  return all[0].command;
});

await rec.step(p, "Pierlink 开着时在带空格和中文的文件夹里右键打开：新标签里的 PowerShell 就在这个目录", async () => {
  return openedAt(menuEntry(MENU_KEYS[0]).command, MENU_DIR);
});

await rec.step(p, "右键磁盘打开 C:\\（命令行里是 \"C:\\\"，反斜杠会转义引号）：新标签在 C:\\", async () => {
  return openedAt(menuEntry(MENU_KEYS[2]).command, "C:\\");
});

await rec.step(p, "默认终端改成 Git Bash 后从右键菜单打开：Git Bash 留在这个目录（登录脚本不切回主目录）", async () => {
  const dir = join(HOME, "pl e2e gitbash");
  mkdirSync(dir, { recursive: true });
  const combo = '[role=combobox][aria-label="Default local terminal"]';
  const pick = async (name) => {
    await u.clickText("Settings", "nav");
    // 设置页显示出来（隐藏时下拉框量出来是 0 宽，点了也没用）、下拉列表打开以后再点
    await u.waitFor(`(document.querySelector(${JSON.stringify(combo)})?.getBoundingClientRect().width || 0) > 0`, "设置页的默认终端下拉框", 10000);
    await u.click(combo);
    await u.waitFor(`!!document.querySelector('[role=listbox]')`, "下拉列表", 5000);
    await u.clickText(name, "[role=listbox]");
    await u.waitFor(`(document.querySelector(${JSON.stringify(combo)})?.textContent || '').trim().startsWith(${JSON.stringify(name)})`, `默认终端选中 ${name}`, 5000);
    await u.clickText("Workspace", "nav");
  };
  await pick("Git Bash");
  try {
    const before = await counts(p);
    runMenuCommand(menuEntry(MENU_KEYS[1]).command, dir);
    await u.waitFor(`document.querySelectorAll('.tab').length === ${before.tabs + 1}`, "多出一个标签", 20000);
    await u.waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); return /\\$ *$/m.test(s[0]?.querySelector('.xterm-rows')?.innerText||'');})()`, "Git Bash 提示符", 30000);
    await u.typeLine("echo PL-PWD=$(pwd)");
    const pwd = await u.waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); const m=(s[0]?.querySelector('.xterm-rows')?.innerText||'').match(/^PL-PWD=(.+)$/m); return m && m[1].trim();})()`, "pwd 的输出", 15000);
    if (!pwd.endsWith("/pl e2e gitbash")) throw new Error(`Git Bash 的目录是 ${pwd}，应在 ${dir}`);
    await closeCurrentTab(before.tabs);
    return pwd;
  } finally {
    await pick("Automatic");
  }
});

await rec.step(p, "设置里关掉“Explorer context menu”：三处菜单项都删掉；再打开又都注册回来", async () => {
  const sel = `[role=switch][aria-label="Explorer context menu"]`;
  await u.clickText("Settings", "nav");
  await u.waitFor(`document.querySelector('${sel}')?.getAttribute('aria-checked') === 'true'`, "开关默认打开");
  await u.click(sel);
  await menuState(false);
  await u.click(sel);
  await menuState(true);
  await u.clickText("Workspace", "nav");
  return "关掉后删除、打开后恢复";
});

await rec.step(() => p, "Pierlink 没开时从右键菜单打开：启动后直接在这个目录开一个本机终端", async () => {
  const cmd = menuEntry(MENU_KEYS[0]).command;
  const before = await counts(p);
  p.close();
  kill();
  await sleep(2000);
  runMenuCommand(cmd, MENU_DIR, { ...process.env, PIERLINK_E2E_DEVTOOLS_PORT: String(PORT) });
  p = await connect(PORT, 90000);
  u = ui(p);
  await u.waitFor(`!!document.querySelector('.tabbar')`, "工作区加载完成", 60000);
  // 上次的标签先恢复出来，新标签加在后面并切过去
  await u.waitFor(`document.querySelectorAll('.tab').length === ${before.tabs + 1}`, "恢复的标签之外多出一个", 30000);
  const prompt = await u.waitFor(visiblePrompt, "新标签里的 PowerShell 提示符", 30000);
  if (prompt.toLowerCase() !== MENU_DIR.toLowerCase()) throw new Error(`起始目录是 ${prompt}，应为 ${MENU_DIR}`);
  // 关掉它：后面的阶段接着用这次的标签布局
  await closeCurrentTab(before.tabs);
  return prompt;
});

if (p.errors.length) console.log("页面错误：", p.errors.join("\n"));
p.close();
kill();
process.exitCode = rec.finish() ? 1 : 0;
