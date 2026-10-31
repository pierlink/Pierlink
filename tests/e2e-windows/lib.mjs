// Windows 端到端测试的公共工具：连接真实 Pierlink（WebView2 调试端口），真实鼠标键盘输入，截图。
// 只用 Node 自带的 fetch / WebSocket，不需要 npm 依赖。
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { mkdirSync, openSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * 连接 Pierlink 的页面（WebView2 用 --remote-debugging-port 启动）。
 * @param port 调试端口
 * @param timeoutMs 等待应用启动的最长时间
 */
export async function connect(port, timeoutMs = 60000) {
  const deadline = Date.now() + timeoutMs;
  let target;
  while (!target) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
      target = list.find((t) => t.type === "page" && t.webSocketDebuggerUrl);
    } catch { /* 应用还没启动 */ }
    if (!target) {
      if (Date.now() > deadline) throw new Error(`等待 Pierlink 调试端口超时：端口=${port}`);
      await sleep(500);
    }
  }
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0;
  const pending = new Map();
  const errors = [];
  ws.onmessage = (m) => {
    const d = JSON.parse(m.data);
    if (d.id && pending.has(d.id)) { pending.get(d.id)(d); pending.delete(d.id); }
    if (d.method === "Runtime.exceptionThrown") errors.push(d.params.exceptionDetails.text);
    if (d.method === "Runtime.consoleAPICalled" && d.params.type === "error") errors.push(JSON.stringify(d.params.args.map((a) => a.value ?? a.description)));
  };
  // 连接已关闭时立即返回；每个请求最多等 30 秒（应用卡住或重启时测试不会一直挂着）
  const send = (method, params = {}) => new Promise((r) => {
    if (ws.readyState !== WebSocket.OPEN) { r({ error: "连接已关闭" }); return; }
    const i = ++id;
    const timer = setTimeout(() => { pending.delete(i); r({ error: `请求超时：${method}` }); }, 30000);
    pending.set(i, (d) => { clearTimeout(timer); r(d); });
    ws.send(JSON.stringify({ id: i, method, params }));
  });
  const js = async (expr) => {
    const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
    if (r.result?.exceptionDetails) throw new Error(`页面脚本出错：${r.result.exceptionDetails.text} ${r.result.exceptionDetails.exception?.description ?? ""}`);
    return r.result?.result?.value;
  };
  await send("Page.enable");
  await send("Runtime.enable");
  return { ws, send, js, errors, close: () => ws.close() };
}

/** 页面操作的封装：按选择器找元素、真实点击、键盘输入、等待条件 */
export function ui(p) {
  const { send, js } = p;
  // 先把元素滚进视野（只滚到刚好看见）：长页面和对话框里屏幕外的元素按坐标点不到
  const rect = (sel, i = 0) => js(`(()=>{const e=document.querySelectorAll(${JSON.stringify(sel)})[${i}]; if(!e) return null; e.scrollIntoView({block:'nearest',inline:'nearest'}); const r=e.getBoundingClientRect(); return {x:r.x,y:r.y,w:r.width,h:r.height};})()`);
  const mouse = (type, x, y, button = "left", buttons = 1, clickCount = 1) => send("Input.dispatchMouseEvent", { type, x, y, button, buttons, clickCount });
  const clickXY = async (x, y, button = "left", clickCount = 1) => {
    await mouse("mouseMoved", x, y, "none", 0);
    await mouse("mousePressed", x, y, button, button === "right" ? 2 : 1, clickCount);
    await mouse("mouseReleased", x, y, button, 0, clickCount);
    await sleep(200);
  };
  const click = async (sel, i = 0, button = "left", clickCount = 1) => {
    const r = await rect(sel, i);
    if (!r) throw new Error(`找不到元素：${sel}[${i}]`);
    await clickXY(r.x + r.w / 2, r.y + r.h / 2, button, clickCount);
  };
  /** 点击文字为 text 的按钮（在 scope 里找，精确匹配或以它开头） */
  const clickText = async (text, scope = "") => {
    const r = await js(`(()=>{const b=[...document.querySelectorAll('${scope} button, ${scope} [role=menuitem], ${scope} [role=option], ${scope} [role=radio]')].find(x=>{const t=x.textContent.trim(); return t===${JSON.stringify(text)}||t.startsWith(${JSON.stringify(text)});}); if(!b) return null; b.scrollIntoView({block:'nearest',inline:'nearest'}); const r=b.getBoundingClientRect(); return {x:r.x,y:r.y,w:r.width,h:r.height};})()`);
    if (!r) throw new Error(`找不到按钮：${text}（范围 ${scope || "整页"}）`);
    await clickXY(r.x + r.w / 2, r.y + r.h / 2);
  };
  const drag = async (x1, y1, x2, y2, steps = 12) => {
    await mouse("mouseMoved", x1, y1, "none", 0);
    await mouse("mousePressed", x1, y1);
    for (let i = 1; i <= steps; i++) { await mouse("mouseMoved", x1 + ((x2 - x1) * i) / steps, y1 + ((y2 - y1) * i) / steps); await sleep(40); }
    await sleep(150);
    await mouse("mouseReleased", x2, y2, "left", 0);
    await sleep(400);
  };
  /** 输入文字（中文等也可以） */
  const type = (text) => send("Input.insertText", { text });
  /** 按一个键；modifiers：Alt=1 Ctrl=2 Meta=4 Shift=8 */
  const key = async (k, code, vk, modifiers = 0, text) => {
    await send("Input.dispatchKeyEvent", { type: "rawKeyDown", key: k, code, windowsVirtualKeyCode: vk, modifiers });
    if (text) await send("Input.dispatchKeyEvent", { type: "char", text, key: k, modifiers });
    await send("Input.dispatchKeyEvent", { type: "keyUp", key: k, code, windowsVirtualKeyCode: vk, modifiers });
    await sleep(120);
  };
  const enter = () => key("Enter", "Enter", 13, 0, "\r");
  const esc = () => key("Escape", "Escape", 27);
  /** 等到 expr 为真（返回它的值），超时抛错 */
  const waitFor = async (expr, what, timeoutMs = 15000) => {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const v = await js(expr).catch(() => null);
      if (v) return v;
      if (Date.now() > deadline) throw new Error(`等待超时（${timeoutMs / 1000} 秒）：${what}`);
      await sleep(250);
    }
  };
  const dialogText = () => js(`[...document.querySelectorAll('.pl-dialog')].map(d=>d.innerText.replace(/\\s+/g,' ')).join(' || ')`);
  /** 当前可见窗格里终端的文字（DOM 渲染器） */
  /** 在当前终端里输入一行命令：等文字真的出现在终端里再按回车（否则回车可能先于文字到达） */
  const typeLine = async (text) => {
    await type(text);
    const tail = text.slice(-12);
    // 去掉换行再找：窗格窄时一行命令会折到下一行
    await waitFor(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); return s.some(x=>(x.querySelector('.xterm-rows')?.innerText||'').replace(/\\n/g,'').includes(${JSON.stringify(tail)}));})()`, `终端里出现输入的文字：${tail}`, 10000);
    await sleep(150);
    await enter();
  };
  const termText = (i = 0) => js(`(()=>{const s=[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none'); const r=s[${i}]?.querySelector('.xterm-rows'); return r ? r.innerText : null;})()`);
  return { rect, mouse, clickXY, click, clickText, drag, type, typeLine, key, enter, esc, waitFor, dialogText, termText };
}

/** 测试记录：每步通过/失败、截图、最后写成汇总文件 */
export function recorder(outDir) {
  mkdirSync(outDir, { recursive: true });
  const results = [];
  let n = 0;
  return {
    results,
    /**
     * 跑一步：失败只记录，不中断后面的步骤（后面的步骤自己判断前提是否满足）
     * @param target 连接，或返回当前连接的函数
     * @returns 这一步是否通过
     */
    async step(target, name, fn) {
      const no = String(++n).padStart(2, "0");
      const started = Date.now();
      let ok = true;
      let detail = "";
      // 一步最多 10 分钟：卡住时只记这一步失败，后面的步骤照常跑，日志和截图也能留下来
      let timer;
      try {
        detail = (await Promise.race([
          fn(),
          new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("这一步超过 10 分钟没有结束")), 10 * 60 * 1000); }),
        ])) ?? "";
      } catch (e) {
        ok = false;
        detail = e?.stack ?? String(e);
      } finally {
        clearTimeout(timer);
      }
      try {
        // target 可以是连接，或返回当前连接的函数（应用重启后连接会换）
        const p = typeof target === "function" ? target() : target;
        const shot = await p.send("Page.captureScreenshot", { format: "png" });
        if (shot.result?.data) writeFileSync(join(outDir, `${no}-${ok ? "通过" : "失败"}-${name.replace(/[\\/:*?"<>|\s]+/g, "_")}.png`), Buffer.from(shot.result.data, "base64"));
      } catch { /* 截图失败不影响结果 */ }
      const line = `${ok ? "✅" : "❌"} ${no} ${name}（${((Date.now() - started) / 1000).toFixed(1)} 秒）${detail ? `\n   ${String(detail).replace(/\n/g, "\n   ")}` : ""}`;
      console.log(line);
      results.push({ no, name, ok, detail: String(detail) });
      return ok;
    },
    finish() {
      const failed = results.filter((r) => !r.ok);
      const text = results.map((r) => `${r.ok ? "✅" : "❌"} ${r.no} ${r.name}${r.detail ? `\n   ${r.detail.replace(/\n/g, "\n   ")}` : ""}`).join("\n");
      writeFileSync(join(outDir, "结果.txt"), `${text}\n\n通过 ${results.length - failed.length} / ${results.length}\n`);
      console.log(`\n通过 ${results.length - failed.length} / ${results.length}`);
      return failed.length;
    },
  };
}

/**
 * 启动 Pierlink（打开 WebView2 调试端口）并等工作区加载完成。
 * @param exe Pierlink.exe 完整路径
 * @param outDir 应用输出日志放在这里（启动失败时看原因）
 * @param port 调试端口
 * @returns { p: 连接, u: 页面操作, pid: 启动的进程号 }
 */
export async function launchApp(exe, outDir, port) {
  mkdirSync(outDir, { recursive: true });
  const log = openSync(join(outDir, `应用输出-${Date.now()}.log`), "a");
  const child = spawn(exe, [], {
    detached: true,
    stdio: ["ignore", log, log],
    // WebView2 的环境变量会被 Tauri 自带的浏览器参数盖掉，用应用自己的测试开关
    env: { ...process.env, PIERLINK_E2E_DEVTOOLS_PORT: String(port) },
  });
  child.unref();
  let p;
  try {
    p = await connect(port, 90000);
  } catch (e) {
    diagnose(child.pid, outDir);
    throw e;
  }
  const u = ui(p);
  await u.waitFor(`!!document.querySelector('.tabbar')`, "工作区加载完成", 60000);
  return { p, u, pid: child.pid };
}

/** 运行一段 PowerShell，返回标准输出（去掉首尾空白） */
export function pwsh(cmd) {
  const r = spawnSync("pwsh.exe", ["-NoProfile", "-NonInteractive", "-Command", cmd], { encoding: "utf8" });
  return `${(r.stdout || "").trim()}${r.stderr ? `\n（错误输出）${r.stderr.trim()}` : ""}${r.error ? `\n（启动失败）${r.error.message}` : ""}`;
}

/** 连不上调试端口时打印诊断：进程、监听端口、WebView2 写的调试端口文件、桌面截图 */
export function diagnose(pid, outDir) {
  console.log("诊断：启动的进程 PID =", pid);
  console.log("诊断：相关进程\n" + pwsh("Get-Process | Where-Object { $_.ProcessName -match 'pier|webview' } | ForEach-Object { '{0} {1} 会话={2} 窗口=[{3}]' -f $_.Id, $_.ProcessName, $_.SessionId, $_.MainWindowTitle }"));
  console.log("诊断：本 Node 进程的会话 = " + pwsh("(Get-Process -Id " + process.pid + ").SessionId"));
  console.log("诊断：监听的端口\n" + pwsh("Get-NetTCPConnection -State Listen | ForEach-Object { '{0}:{1} 进程={2}' -f $_.LocalAddress, $_.LocalPort, $_.OwningProcess }"));
  console.log("诊断：WebView2 调试端口文件\n" + pwsh("Get-ChildItem -Path $env:LOCALAPPDATA -Recurse -Filter DevToolsActivePort -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName + ' = ' + ((Get-Content $_.FullName) -join ' ') }"));
  console.log("诊断：WebView2 数据目录\n" + pwsh("Get-ChildItem -Path $env:LOCALAPPDATA -Directory -Recurse -Filter EBWebView -ErrorAction SilentlyContinue | ForEach-Object FullName"));
  console.log("诊断：msedgewebview2 命令行\n" + pwsh("Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | Select-Object -First 2 | ForEach-Object CommandLine"));
  const shot = join(outDir, "诊断-桌面截图.png");
  console.log("诊断：桌面截图 " + pwsh(`Add-Type -AssemblyName System.Windows.Forms,System.Drawing; $b=[System.Windows.Forms.Screen]::PrimaryScreen.Bounds; $bmp=New-Object System.Drawing.Bitmap $b.Width,$b.Height; $g=[System.Drawing.Graphics]::FromImage($bmp); $g.CopyFromScreen($b.Location,[System.Drawing.Point]::Empty,$b.Size); $bmp.Save('${shot}'); 'saved ' + $b.Width + 'x' + $b.Height`));
}

/** 强制结束 Pierlink（模拟崩溃或直接关机） */
export function killApp(exe) {
  try { execFileSync("taskkill", ["/IM", exe.split(/[\\/]/).pop(), "/F"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
}
