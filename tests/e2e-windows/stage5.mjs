// 阶段 5 的 Windows 端到端测试：升级搬数据、关窗口缩到托盘、退出确认、卸载后数据去留。
// 分几个阶段运行，中间由 CI 装/卸安装包（见 .github/workflows/ci.yml 的 e2e-windows 任务）：
//   E2E_PHASE=old          装好旧版后：加一台服务器，看数据在哪（0.1.0 在旧位置，0.3 起在新位置）
//   E2E_PHASE=upgrade      覆盖安装新版后：服务器还在、数据在新位置；内置 SSH 的说明；托盘；退出确认
//   E2E_PHASE=reinstalled  静默卸载（不删数据）再重装后：服务器和设置都还在，说明不再出现
//
// 环境变量：PIERLINK_EXE（必填）、E2E_OUT（必填）、E2E_PHASE（必填）
// 每步截图；某一步失败不中断后面的步骤。全部通过时退出码为 0。
import { existsSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { killApp, launchApp, pwsh, recorder, sleep } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const PHASE = process.env.E2E_PHASE;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, `阶段5-${PHASE}`);
const PORT = 9222;
if (!EXE || !OUT || !["old", "upgrade", "reinstalled"].includes(PHASE)) {
  throw new Error(`缺少或不认识的环境变量：PIERLINK_EXE=${EXE} E2E_OUT=${OUT} E2E_PHASE=${PHASE}`);
}

const LOCAL = process.env.LOCALAPPDATA;
/** 旧版的数据目录（和安装目录相同） */
const OLD_DATA = join(LOCAL, "Pierlink");
/** 新版的数据目录 */
const NEW_DATA = join(LOCAL, "com.pierlink.desktop");
/** 升级测试用的服务器（不真的连接，只看数据是否保留） */
const SERVER_NAME = "迁移测试";
const SERVER_HOST = "192.0.2.10";

const rec = recorder(OUT);
const { p, u } = await launchApp(EXE, OUT, PORT);

/** 列出 Pierlink 进程的顶层窗口：每行“可见(1/0) 类名 标题” */
const LIST_WINDOWS = `
$p = Get-Process -Name Pierlink -ErrorAction SilentlyContinue
if (-not $p) { 'gone'; exit }
Add-Type -Namespace W -Name U -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc f, IntPtr l);
public delegate bool EnumProc(IntPtr h, IntPtr l);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
'@
$ids = @($p | ForEach-Object Id)
$out = [System.Collections.Generic.List[string]]::new()
[W.U]::EnumWindows({ param($h, $l)
  [uint32]$id = 0; [void][W.U]::GetWindowThreadProcessId($h, [ref]$id)
  if ($ids -contains $id) {
    $t = New-Object System.Text.StringBuilder 256; [void][W.U]::GetWindowText($h, $t, 256)
    $c = New-Object System.Text.StringBuilder 256; [void][W.U]::GetClassName($h, $c, 256)
    $out.Add(('{0} {1} {2}' -f [int][W.U]::IsWindowVisible($h), $c, $t))
  }
  $true }, [IntPtr]::Zero) | Out-Null
$out
`;

/** Pierlink 主窗口状态：gone（进程没了）/ hidden / visible；按标题为 Pierlink 的顶层窗口判断 */
function windowState() {
  const lines = pwsh(LIST_WINDOWS).split("\n").map((l) => l.trim()).filter(Boolean);
  if (lines[0] === "gone") return { state: "gone", lines };
  const main = lines.filter((l) => / Pierlink$/.test(l));
  return { state: main.some((l) => l.startsWith("1 ")) ? "visible" : "hidden", lines };
}

async function waitWindow(want, what, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  let now;
  while ((now = windowState()).state !== want) {
    if (Date.now() > deadline) throw new Error(`等待超时：${what}（现在是 ${now.state}；顶层窗口：${now.lines.join(" | ")}）`);
    await sleep(300);
  }
}

/** 标题栏最右边的关闭按钮（文字随“关闭时缩到托盘”设置变化） */
const closeButton = () => p.js(`(()=>{const b=[...document.querySelectorAll('header button')].pop(); const r=b.getBoundingClientRect(); return {label:b.getAttribute('aria-label'), x:r.x+r.width/2, y:r.y+r.height/2};})()`);

if (PHASE === "old") {
  await rec.step(p, "旧版：添加一台服务器（只保存，不连接）", async () => {
    await u.click('[aria-label="Add server"]');
    await u.waitFor(`!!document.querySelector('.pl-dialog')`, "添加服务器对话框");
    const fill = async (label, value) => {
      const ok = await p.js(`(()=>{const l=[...document.querySelectorAll('.pl-dialog label.pl-field')].find(l=>l.textContent.trim().startsWith(${JSON.stringify(label)})); const i=l?.querySelector('input'); if(!i) return false; i.focus(); i.select(); return true;})()`);
      if (!ok) throw new Error(`找不到输入框：${label}`);
      await u.type(value);
    };
    await fill("Host", SERVER_HOST);
    await fill("User", "pltest");
    await fill("Display name", SERVER_NAME);
    await u.clickText("Save", ".pl-dialog");
    await u.waitFor(`!document.querySelector('.pl-dialog')`, "对话框关闭");
    await u.waitFor(`[...document.querySelectorAll('.srv-row')].some(r=>r.innerText.includes(${JSON.stringify(SERVER_NAME)}))`, "服务器出现在列表里");
  });
  await rec.step(p, "旧版：数据库的位置（0.1.0 在安装目录；0.3 起已在新位置）", async () => {
    // 写入是异步的，给一点时间
    await sleep(1000);
    if (existsSync(join(OLD_DATA, "pierlink.db"))) return `旧位置：${OLD_DATA}`;
    if (existsSync(join(NEW_DATA, "pierlink.db"))) return `已在新位置：${NEW_DATA}`;
    throw new Error(`两个位置都没有数据库：${OLD_DATA}、${NEW_DATA}`);
  });
  killApp(EXE);
}

if (PHASE === "upgrade") {
  await rec.step(p, "升级后：服务器还在，数据已搬到新位置，旧位置没有数据库", async () => {
    await u.waitFor(`[...document.querySelectorAll('.srv-row')].some(r=>r.innerText.includes(${JSON.stringify(SERVER_NAME)}))`, "旧版加的服务器出现在列表里", 15000);
    if (!existsSync(join(NEW_DATA, "pierlink.db"))) throw new Error(`新位置没有数据库：${NEW_DATA}`);
    if (existsSync(join(OLD_DATA, "pierlink.db"))) throw new Error(`旧位置的数据库还在：${OLD_DATA}`);
    return `新位置=${NEW_DATA}`;
  });

  await rec.step(p, "升级后：服务器列表顶部说明一次内置 SSH（不是弹窗），点“知道了”后消失", async () => {
    const text = await u.waitFor(`document.querySelector('.srv-notice')?.innerText`, "内置 SSH 的升级说明", 15000);
    if (!/Pierlink now has built-in SSH/.test(text) || !/The server you added in Pierlink now uses built-in SSH/.test(text)) throw new Error(`升级说明的文字不对：${text}`);
    if (await p.js(`!!document.querySelector('.pl-dialog')`)) throw new Error("升级说明不该弹窗");
    await u.clickText("Got it", ".srv-notice");
    await u.waitFor(`!document.querySelector('.srv-notice')`, "点“知道了”后说明消失");
    return text.replace(/\s+/g, " ").slice(0, 120);
  });

  await rec.step(p, "关闭窗口缩到托盘：进程还在、窗口隐藏；再次打开程序窗口回来", async () => {
    const btn = await closeButton();
    if (!/tray/i.test(btn.label)) throw new Error(`关闭按钮不是“缩到托盘”（系统托盘不可用？）：${btn.label}`);
    await waitWindow("visible", "窗口一开始可见");
    await u.clickXY(btn.x, btn.y);
    await waitWindow("hidden", "点关闭后窗口隐藏、进程还在");
    // 再启动一次：单实例，应该把已有窗口叫出来，而不是开第二个
    pwsh(`Start-Process '${EXE.replace(/'/g, "''")}'`);
    await waitWindow("visible", "再次打开程序后窗口回来");
    const count = pwsh("(Get-Process -Name Pierlink -ErrorAction SilentlyContinue | Measure-Object).Count").split("\n")[0].trim();
    if (count !== "1") throw new Error(`Pierlink 进程不止一个：${count}`);
  });

  await rec.step(p, "关掉“关闭时缩到托盘”后点关闭：有终端时弹出退出确认，点“退出”后程序结束", async () => {
    await u.clickText("Settings", "nav");
    await u.waitFor(`!!document.querySelector('[role=switch][aria-label^="Minimize to tray"]')`, "设置页的托盘开关");
    const on = await p.js(`document.querySelector('[role=switch][aria-label^="Minimize to tray"]').getAttribute('aria-checked')`);
    if (on === "true") await u.click('[role=switch][aria-label^="Minimize to tray"]');
    await u.waitFor(`document.querySelector('[role=switch][aria-label^="Minimize to tray"]').getAttribute('aria-checked')==='false'`, "开关已关");
    await u.waitFor(`/Quit/.test([...document.querySelectorAll('header button')].pop().getAttribute('aria-label'))`, "关闭按钮变成“退出”");
    // 开一个本机终端：有终端要关时一定要先确认（什么都没开时会直接退出，那种情况测不到对话框）
    await u.clickText("Workspace", "nav");
    if (!(await p.js(`document.querySelectorAll('.pane-slot').length`))) await u.clickText("Default local terminal");
    await u.waitFor(`(()=>{const r=document.querySelector('.xterm-rows'); return r && /PS |>/.test(r.innerText);})()`, "本机终端出现提示符", 30000);
    const btn = await closeButton();
    await u.clickXY(btn.x, btn.y);
    await u.waitFor(`/Quit Pierlink\\?/.test(document.querySelector('.pl-dialog')?.innerText||'') && /terminal session/.test(document.querySelector('.pl-dialog').innerText)`, "退出确认对话框（写着有终端会被关闭）", 15000);
    const shot = await p.send("Page.captureScreenshot", { format: "png" });
    if (shot.result?.data) writeFileSync(join(OUT, "退出确认对话框.png"), Buffer.from(shot.result.data, "base64"));
    // 先点“取消”：程序应当还在
    await u.clickText("Cancel", ".pl-dialog .pl-dialog__actions");
    await u.waitFor(`!document.querySelector('.pl-dialog')`, "取消后对话框关闭");
    await sleep(1000);
    if (windowState().state !== "visible") throw new Error("点取消后程序不应退出");
    // 再关一次，这次点“退出”
    await u.clickXY(btn.x, btn.y);
    await u.waitFor(`/Quit Pierlink\\?/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "再次出现退出确认", 15000);
    await u.clickText("Quit", ".pl-dialog .pl-dialog__actions");
    await waitWindow("gone", "点退出后程序结束", 15000);
  });
}

if (PHASE === "reinstalled") {
  await rec.step(p, "卸载（不删数据）再重装：服务器和设置都还在", async () => {
    await u.waitFor(`[...document.querySelectorAll('.srv-row')].some(r=>r.innerText.includes(${JSON.stringify(SERVER_NAME)}))`, "旧版加的服务器还在", 15000);
    // 升级时已经点过“知道了”：重装后不再说明
    await sleep(2000);
    if (await p.js(`!!document.querySelector('.srv-notice')`)) throw new Error("点过“知道了”的内置 SSH 说明在重装后又出现了");
    const label = (await closeButton()).label;
    if (!/Quit/.test(label)) throw new Error(`“关闭时缩到托盘”设置没保留：关闭按钮=${label}`);
  });
  killApp(EXE);
}

p.close();
process.exitCode = rec.finish() ? 1 : 0;
