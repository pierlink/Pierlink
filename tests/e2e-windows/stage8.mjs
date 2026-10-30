// 0.5.0 的 Windows 端到端测试：AI 接入（MCP）。
// 用 node 脚本当 AI 助手：启动安装好的 `Pierlink.exe --mcp`，按 MCP 协议发请求（旧式握手和 2026-07-28 新式各一遍）；
// 界面上的确认框、AI 终端标记、全部停止用 Chrome 调试协议真实点击和检查。服务器入口从 WSL 里带令牌调用。
// 在 stage7.mjs 之后运行：那里已经有 WSL 里的 Linux 服务器 pl-linux（密钥登录、已信任指纹）。
//
// 环境变量：PIERLINK_EXE、E2E_OUT（必填）；PIERLINK_TEST_LINUX_*（同 stage6.mjs，没有时跳过服务器相关步骤）
// 每一步截图；某一步失败不中断后面的步骤。全部通过时退出码为 0。
import net from "node:net";
import { join } from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { existsSync, mkdirSync, openSync, writeFileSync } from "node:fs";
import { killApp, launchApp, recorder, sleep } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, "0.5.0");
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
/** 文件工具允许的文件夹（测试临时目录里） */
const FOLDER = join(OUT, "mcp-files");
mkdirSync(FOLDER, { recursive: true });
const MODERN = "2026-07-28";
const meta = (version = MODERN) => ({
  "io.modelcontextprotocol/protocolVersion": version,
  "io.modelcontextprotocol/clientInfo": { name: "e2e 新式客户端", version: "1" },
  "io.modelcontextprotocol/clientCapabilities": {},
});

const rec = recorder(OUT);
killApp(EXE);
await sleep(1000);
const { p, u } = await launchApp(EXE, OUT, PORT);

// WSL 里的 sshd 由本脚本启动、测完结束
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

/** 一个按 MCP stdio 方式说话的客户端：启动 `Pierlink.exe --mcp`，一行一个 JSON */
class Agent {
  constructor() {
    this.proc = spawn(EXE, ["--mcp"], { stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
    this.buf = "";
    this.stderr = "";
    this.next = 0;
    this.waiting = new Map();
    this.exited = null;
    this.proc.stdout.on("data", (d) => {
      this.buf += d.toString("utf8");
      for (let i; (i = this.buf.indexOf("\n")) >= 0;) {
        const line = this.buf.slice(0, i);
        this.buf = this.buf.slice(i + 1);
        let msg;
        try { msg = JSON.parse(line); } catch { continue; }
        const done = msg.id !== undefined && this.waiting.get(msg.id);
        if (done) { this.waiting.delete(msg.id); done(msg); }
      }
    });
    this.proc.stderr.on("data", (d) => { this.stderr += d.toString("utf8"); });
    this.proc.on("exit", (code) => {
      this.exited = code;
      for (const done of this.waiting.values()) done({ error: { message: `桥接程序已退出（${code}）` } });
      this.waiting.clear();
    });
  }
  request(method, params = {}, timeoutMs = 60000) {
    const id = ++this.next;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.waiting.delete(id); reject(new Error(`等待 ${method} 超时；桥接程序输出：${this.stderr.slice(-500)}`)); }, timeoutMs);
      this.waiting.set(id, (msg) => { clearTimeout(timer); resolve(msg); });
      this.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    });
  }
  notify(method, params = {}) { this.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`); }
  call(name, args = {}, timeoutMs) { return this.request("tools/call", { name, arguments: args }, timeoutMs); }
  async initialize(version = "2025-11-25") {
    const r = await this.request("initialize", { protocolVersion: version, capabilities: {}, clientInfo: { name: "e2e 旧式客户端", version: "1" } });
    if (r.error) throw new Error(`握手失败：${JSON.stringify(r.error)}`);
    this.notify("notifications/initialized");
    return r.result;
  }
  close() { try { this.proc.stdin.end(); } catch { /* 已退出 */ } }
}

/** 工具结果里的结构化内容；工具报错时抛出它给模型看的文字 */
function structured(r) {
  if (r.error) throw new Error(`请求出错：${JSON.stringify(r.error)}`);
  if (r.result?.isError) throw new Error(`工具返回错误：${r.result.content?.[0]?.text}`);
  return r.result.structuredContent;
}
/** 工具报错时给模型看的文字；没报错时抛出 */
function toolError(r) {
  if (!r.result?.isError) throw new Error(`应该报错却成功了：${JSON.stringify(r).slice(0, 300)}`);
  return r.result.content?.[0]?.text ?? "";
}

/** 等确认框出现，点 label 开头的按钮；返回框里的文字 */
async function answerConfirm(label, what) {
  const text = await u.waitFor(`(()=>{const d=document.querySelector('.pl-dialog .mcp-confirm')?.closest('.pl-dialog'); return d ? d.innerText : null;})()`, what, 30000);
  await u.clickText(label, ".pl-dialog");
  await u.waitFor(`!document.querySelector('.pl-dialog .mcp-confirm')`, "确认框关闭", 10000);
  return text.replace(/\s+/g, " ");
}

/** 打开顶部导航的 MCP 页（入口列表） */
async function openMcpPage() {
  await u.clickText("MCP", "header nav");
  await u.waitFor(`!!document.querySelector('.mcp-page')`, "MCP 页");
}
const switchOn = (label) => p.js(`document.querySelector('[role=switch][aria-label=${JSON.stringify(label)}]')?.getAttribute('aria-checked')==='true'`);
async function turnOn(label) {
  if (await switchOn(label)) return;
  await u.click(`[role=switch][aria-label=${JSON.stringify(label)}]`);
  await u.waitFor(`document.querySelector('[role=switch][aria-label=${JSON.stringify(label)}]')?.getAttribute('aria-checked')==='true'`, `打开“${label}”`);
}
/** 当前选中入口的详情里，勾选标签文字为 label 的复选框 */
async function check(label) {
  const ok = await p.js(`(()=>{const l=[...document.querySelectorAll('.mcp-detail label.pl-check')].find(l=>l.textContent.trim()===${JSON.stringify(label)}); const i=l?.querySelector('input'); if(!i) return null; if(!i.checked) i.click(); return true;})()`);
  if (!ok) throw new Error(`找不到复选框：${label}`);
  await sleep(500);
}
/** 当前选中入口的详情里，加一个允许的文件夹 */
async function addFolder(folder) {
  const ok = await p.js(`(()=>{const i=document.querySelector('.mcp-detail .mcp-perms__add input'); if(!i) return false; i.focus(); i.select(); return true;})()`);
  if (!ok) throw new Error("找不到文件夹输入框");
  await u.type(folder);
  await u.clickText("Add", ".mcp-detail .mcp-perms__add");
  await u.waitFor(`[...document.querySelectorAll('.mcp-detail .mcp-perms__folder')].some(f=>f.innerText.includes(${JSON.stringify(folder)}))`, "文件夹出现在列表里");
}

await rec.step(p, "MCP 页打开总开关和本机入口：允许执行命令、一个文件夹和 pl-linux", async () => {
  await openMcpPage();
  await turnOn("Turn on AI access");
  await u.waitFor(`!!document.querySelector('.mcp-detail')`, "本机入口的详情");
  await turnOn("Local entry (AI on this PC)");
  await addFolder(FOLDER);
  if (HAVE_LINUX) await check(LX_NAME);
  const cmd = await u.waitFor(`[...document.querySelectorAll('.mcp-detail .mcp-cmd code')].map(c=>c.textContent).find(t=>t.startsWith('claude mcp add'))`, "接入命令");
  if (!cmd.includes("--mcp")) throw new Error(`接入命令不对：${cmd}`);
  return cmd;
});

let agent = null;
await rec.step(p, "旧式客户端：握手和工具列表立即回答；第一次调用工具时弹确认框，点“总是允许”", async () => {
  agent = new Agent();
  const info = await agent.initialize("2025-11-25");
  if (info.protocolVersion !== "2025-11-25" || info.serverInfo?.name !== "pierlink") throw new Error(`握手结果不对：${JSON.stringify(info)}`);
  const tools = (await agent.request("tools/list")).result?.tools ?? [];
  const expected = ["list_targets", "run_command", "open_terminal", "list_terminals", "close_terminal", "terminal_send", "terminal_read", "terminal_screen", "terminal_wait", "read_file", "write_file", "edit_file", "search", "find_files", "list_dir", "job_output", "job_stop", "list_jobs", "sync_status", "sync_wait"];
  const toolNames = tools.map((t) => t.name);
  if (toolNames.length !== expected.length || expected.some((n) => !toolNames.includes(n))) throw new Error(`工具不对：${toolNames.join(",")}`);
  const pending = agent.call("list_targets");
  const dialog = await answerConfirm("Always allow", "第一次连接的确认框");
  let targets = structured(await pending);
  let names = targets.map((t) => t.target);
  // 前一步刚保存的入口权限可能还没生效：最多等 10 秒
  for (let i = 0; i < 20 && HAVE_LINUX && !names.includes(LX_NAME); i++) {
    await sleep(500);
    targets = structured(await agent.call("list_targets"));
    names = targets.map((t) => t.target);
  }
  if (!names.includes("windows") || (HAVE_LINUX && !names.includes(LX_NAME))) throw new Error(`目标不对：${names}`);
  return `${dialog.slice(0, 120)}｜目标：${names.join(", ")}`;
});

await rec.step(p, "run_command：本机 PowerShell 和 WSL 服务器都原样返回输出和退出码", async () => {
  if (!agent) throw new Error("没有客户端");
  const local = structured(await agent.call("run_command", { target: "windows", command: "Write-Output \"pl-mcp-$(40+2)\"; exit 3" }));
  if (!local.stdout.includes("pl-mcp-42") || local.exit_code !== 3) throw new Error(`本机结果不对：${JSON.stringify(local)}`);
  if (!HAVE_LINUX) return `本机：${local.stdout.trim()}（没有 WSL 服务器）`;
  const remote = structured(await agent.call("run_command", { target: LX_NAME, command: "echo pl-mcp-$((40+2)); echo err >&2; exit 5" }));
  if (!remote.stdout.includes("pl-mcp-42") || !remote.stderr.includes("err") || remote.exit_code !== 5) throw new Error(`服务器结果不对：${JSON.stringify(remote)}`);
  return `本机：${local.stdout.trim()}；服务器：${remote.stdout.trim()}`;
});

await rec.step(p, "危险命令先弹框确认；点“拒绝”后不执行", async () => {
  if (!agent) throw new Error("没有客户端");
  const victim = join(OUT, "pl-mcp-danger");
  mkdirSync(victim, { recursive: true });
  writeFileSync(join(victim, "keep.txt"), "keep");
  const pending = agent.call("run_command", { target: "windows", command: `Remove-Item -Recurse -Force '${victim}'` });
  const dialog = await answerConfirm("Deny", "危险命令确认框");
  if (!dialog.includes("Remove-Item")) throw new Error(`确认框里没有命令原文：${dialog}`);
  const text = toolError(await pending);
  if (!existsSync(join(victim, "keep.txt"))) throw new Error("拒绝了却还是删掉了");
  return text;
});

await rec.step(p, "文件工具：允许的文件夹里能读写，外面被拒", async () => {
  if (!agent) throw new Error("没有客户端");
  const file = join(FOLDER, "hello.txt");
  structured(await agent.call("write_file", { file_path: file, content: "你好 MCP\n" }));
  // 只改一处，再按行读回来
  structured(await agent.call("edit_file", { file_path: file, old_string: "MCP", new_string: "Pierlink" }));
  const read = await agent.call("read_file", { file_path: file });
  if (!JSON.stringify(read).includes("你好 Pierlink")) throw new Error(`读回来的内容不对：${JSON.stringify(read).slice(0, 300)}`);
  const list = await agent.call("list_dir", { path: FOLDER });
  if (!JSON.stringify(list).includes("hello.txt")) throw new Error("目录列表里没有 hello.txt");
  const outside = toolError(await agent.call("write_file", { file_path: join(OUT, "outside.txt"), content: "x" }));
  if (existsSync(join(OUT, "outside.txt"))) throw new Error("允许的文件夹外面也写进去了");
  return outside;
});

await rec.step(p, "AI 打开终端：界面出现 AI 标签，发送命令并等到输出", async () => {
  if (!agent) throw new Error("没有客户端");
  if (!HAVE_LINUX) throw new Error("没有 WSL 里的 Linux 服务器");
  await u.clickText("Workspace", "header nav");
  const opened = structured(await agent.call("open_terminal", { target: LX_NAME, cwd: "/tmp" }, 150000));
  const id = opened.terminal_id;
  await u.waitFor(`!!document.querySelector('.ai-badge')`, "标签上的 AI 标记");
  structured(await agent.call("terminal_send", { terminal_id: id, text: "echo pl-ai-$((40+2)) $(pwd)", keys: ["Enter"] }));
  const waited = structured(await agent.call("terminal_wait", { terminal_id: id, pattern: "pl-ai-42 /tmp", timeout_secs: 30 }));
  if (!waited.matched) throw new Error(`没等到输出：${JSON.stringify(waited).slice(0, 300)}`);
  const listed = structured(await agent.call("list_terminals"));
  if (!JSON.stringify(listed).includes(id)) throw new Error("list_terminals 里没有刚打开的终端");
  return `终端 ${id.slice(0, 8)}…`;
});

await rec.step(p, "交给 AI：AI 列出你开的本机终端；交出后 AI 能输入并读到输出；收回后不能再操作", async () => {
  if (!agent) throw new Error("没有客户端");
  await u.clickText("Workspace", "header nav");
  // CI 屏幕只有 1024 宽，工具面板浮在终端上会挡住窗格标题栏右边的按钮：先收起
  if (await p.js(`!!document.querySelector('.ws__tool')`)) await u.click('[aria-label^="Tool panel"]');
  await u.waitFor(`!document.querySelector('.ws__tool')`, "工具面板已收起");
  // 新标签开一个本机 PowerShell
  await u.click('.tabbar .pl-split__main');
  const visiblePane = `[...document.querySelectorAll('.pane-slot')].filter(s=>s.style.display!=='none')`;
  await u.waitFor(`${visiblePane}.some(s=>/PS /.test(s.querySelector('.xterm-rows')?.innerText||''))`, "PowerShell 提示符", 30000);
  const listed = structured(await agent.call("list_terminals"));
  const users = (listed.user_terminals ?? []).filter((t) => t.kind === "windows");
  if (!users.length) throw new Error(`list_terminals 里没有用户的本机终端：${JSON.stringify(listed).slice(0, 300)}`);
  if (users.some((t) => "cwd" in t || "address" in t)) throw new Error(`交出前不应给目录或地址：${JSON.stringify(users)}`);
  // 只有本机入口开着：点窗格标题栏的 AI 按钮直接交出（前面阶段恢复的本机终端也在列表里，交出后按标记找这一个）
  await p.js(`${visiblePane}.map(s=>s.querySelector('.pane__btn[aria-label="Hand to AI…"]')).find(Boolean)?.click()`);
  await u.waitFor(`${visiblePane}.some(s=>!!s.querySelector('.pane__ai'))`, "窗格标题栏显示已交给 AI", 15000);
  const after = structured(await agent.call("list_terminals"));
  const mine = (after.user_terminals ?? []).find((t) => t.handed_over);
  if (!mine) throw new Error(`交出后 list_terminals 里没有标成已交出的终端：${JSON.stringify(after).slice(0, 300)}`);
  if (!(after.ai_terminals ?? []).some((t) => t.terminal_id === mine.terminal_id && "cwd" in t)) throw new Error(`交出后应在 ai_terminals 里并带目录：${JSON.stringify(after).slice(0, 400)}`);
  // 先记下输出位置，再发送；等输出时从这里开始找（输出可能在开始等之前就到了）
  const start = structured(await agent.call("terminal_read", { terminal_id: mine.terminal_id }));
  // 交出后的第一次回车：终端里可能有交出前没回车的文字，要用户确认一次
  const sending = agent.call("terminal_send", { terminal_id: mine.terminal_id, text: 'Write-Output "pl-ho-$(40+2)"', keys: ["Enter"] });
  const unknownConfirm = await answerConfirm("Allow this time", "交出后第一次回车的确认框");
  if (!/unknown|未知/i.test(unknownConfirm)) throw new Error(`确认框没说明内容未知：${unknownConfirm}`);
  structured(await sending);
  const waited = structured(await agent.call("terminal_wait", { terminal_id: mine.terminal_id, pattern: "(?m)^pl-ho-42", since: start.cursor, timeout_secs: 30 }));
  if (!waited.matched) throw new Error(`没等到输出：${JSON.stringify(waited).slice(0, 300)}`);
  // 收回：AI 不能再往里输入，终端还开着
  await u.clickText("Take back", ".pane__ai");
  await u.waitFor(`!${visiblePane}.some(s=>!!s.querySelector('.pane__ai'))`, "AI 标记消失", 15000);
  // 收回后这个终端对 AI 来说不存在了：可能以请求错误或工具错误的形式拒绝，两种都算
  const r = await agent.call("terminal_send", { terminal_id: mine.terminal_id, text: "Write-Output nope", keys: ["Enter"] });
  const denied = r.error ? r.error.message : toolError(r);
  return `交出 ${mine.title}，读到 pl-ho-42；收回后：${denied.slice(0, 80)}`;
});

await rec.step(p, "新式（2026-07-28）客户端：不握手，server/discover 和带 _meta 的调用", async () => {
  const modern = new Agent();
  try {
    const discover = await modern.request("server/discover", { _meta: meta() });
    const versions = discover.result?.supportedVersions ?? [];
    if (!versions.includes(MODERN) || !versions.includes("2025-11-25")) throw new Error(`支持的版本不对：${JSON.stringify(discover).slice(0, 300)}`);
    // 本机入口已经“总是允许”，不再弹框
    const targets = structured(await modern.request("tools/call", { name: "list_targets", arguments: {}, _meta: meta() }));
    const bad = await modern.request("tools/list", { _meta: meta("1900-01-01") });
    if (bad.error?.code !== -32022) throw new Error(`不支持的版本没有回 -32022：${JSON.stringify(bad).slice(0, 300)}`);
    return `版本 ${versions.join(", ")}｜目标 ${targets.length} 个`;
  } finally {
    modern.close();
  }
});

await rec.step(p, "标题栏显示 AI 已连接；点“全部停止”后客户端被断开", async () => {
  if (!agent) throw new Error("没有客户端");
  await u.waitFor(`/AI · [1-9]/.test(document.querySelector('.mcp-btn')?.innerText||'')`, "标题栏的 AI 状态");
  await u.click(".mcp-btn");
  await u.clickText("Stop all", ".mcp-pop");
  const deadline = Date.now() + 15000;
  while (agent.exited === null && Date.now() < deadline) await sleep(250);
  if (agent.exited === null) {
    const r = await agent.call("list_targets", {}, 10000).catch((e) => ({ error: { message: String(e) } }));
    if (!r.error) throw new Error("全部停止后客户端还能调用工具");
  }
  agent.close();
  return `桥接程序退出码 ${agent.exited}`;
});

await rec.step(p, "服务器入口：WSL 里的 AI 带令牌连 127.0.0.1:47821，在 Windows 上执行命令（第一次要确认）", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL 里的 Linux 服务器");
  await openMcpPage();
  // 左边列表“添加服务器入口”：勾上 pl-linux 添加，添加后自动选中它
  await u.clickText("Add server entries", ".mcp-list");
  await u.waitFor(`!!document.querySelector('.pl-dialog .mcp-add')`, "添加服务器入口对话框");
  const picked = await p.js(`(()=>{const l=[...document.querySelectorAll('.pl-dialog .mcp-add label.pl-check')].find(l=>l.textContent.trim().startsWith(${JSON.stringify(LX_NAME)})); const i=l?.querySelector('input'); if(!i) return false; if(!i.checked) i.click(); return true;})()`);
  if (!picked) throw new Error(`添加对话框里没有 ${LX_NAME}`);
  await u.clickText("Add 1", ".pl-dialog");
  const lxRow = `[...document.querySelectorAll('.mcp-row')].find(r=>r.innerText.includes(${JSON.stringify(LX_NAME)}))`;
  await u.waitFor(`!!${lxRow}`, "服务器入口出现在列表里");
  await check("Allow running commands on this PC");
  await u.waitFor(`/Running/.test(${lxRow}?.querySelector('.task-row__detail')?.textContent||'')`, "服务器入口运行中", 60000);
  // 在 WSL 里用 python 当 HTTP 客户端：读 ~/.pierlink/mcp.json 里的地址和令牌。
  // 用 wsl.exe -e 直接执行，不经过 bash（否则脚本里的 $(...) 会先被 bash 当成命令替换）
  const script = [
    "import json, os, urllib.request",
    "cfg = json.load(open(os.path.expanduser('~/.pierlink/mcp.json')))",
    "h = {'Content-Type': 'application/json', 'Accept': 'application/json, text/event-stream', 'Authorization': 'Bearer ' + cfg['token']}",
    "def post(body, extra={}):",
    "    req = urllib.request.Request(cfg['url'], data=json.dumps(body).encode(), headers={**h, **extra}, method='POST')",
    "    with urllib.request.urlopen(req, timeout=120) as r:",
    "        return r.headers.get('Mcp-Session-Id'), r.read().decode()",
    "sid, _ = post({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2025-11-25', 'capabilities': {}, 'clientInfo': {'name': 'wsl-agent', 'version': '1'}}})",
    "extra = {'Mcp-Session-Id': sid, 'MCP-Protocol-Version': '2025-11-25'} if sid else {'MCP-Protocol-Version': '2025-11-25'}",
    "post({'jsonrpc': '2.0', 'method': 'notifications/initialized'}, extra)",
    "_, body = post({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call', 'params': {'name': 'run_command', 'arguments': {'target': 'windows', 'command': 'Write-Output \"pl-srv-$(40+2)\"'}}}, extra)",
    "print(body)",
  ].join("\n");
  const child = spawn("wsl.exe", ["-d", LX.wsl, "-u", LX.user, "-e", "python3", "-c", script], { stdio: ["ignore", "pipe", "pipe"] });
  let out = "";
  let err = "";
  child.stdout.on("data", (d) => { out += d; });
  child.stderr.on("data", (d) => { err += d; });
  const done = new Promise((r) => child.on("exit", r));
  const dialog = await answerConfirm("Allow this time", "服务器入口第一次连接的确认框");
  const code = await Promise.race([done, sleep(120000).then(() => "超时")]);
  if (code !== 0) throw new Error(`WSL 里的客户端失败（${code}）：${err.slice(-500)}`);
  if (!out.includes("pl-srv-42")) throw new Error(`结果里没有 pl-srv-42：${out.slice(0, 500)}`);
  return `${dialog.slice(0, 80)}｜${out.slice(0, 120)}`;
});

await rec.step(p, "服务器入口：令牌不对时被拒绝", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL 里的 Linux 服务器");
  const script = [
    "import json, os, urllib.request, urllib.error",
    "cfg = json.load(open(os.path.expanduser('~/.pierlink/mcp.json')))",
    "req = urllib.request.Request(cfg['url'], data=b'{}', headers={'Content-Type': 'application/json', 'Accept': 'application/json, text/event-stream', 'Authorization': 'Bearer wrong'}, method='POST')",
    "try:",
    "    urllib.request.urlopen(req, timeout=30); print('accepted')",
    "except urllib.error.HTTPError as e:",
    "    print('status', e.code)",
  ].join("\n");
  const out = execFileSync("wsl.exe", ["-d", LX.wsl, "-u", LX.user, "-e", "python3", "-c", script], { encoding: "utf8", timeout: 60000 });
  if (!out.includes("status 401")) throw new Error(`令牌不对时应该 401：${out}`);
  return out.trim();
});

// ---------- 手机入口：在 127.0.0.1 上打开，用 Node 当“手机”走真实 HTTP ----------
let phone = null;
/** “手机”的浏览器标识：配对后电脑上应该显示成 iPhone · Safari */
const PHONE_UA = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1";
/** 读 Windows 剪贴板（经 base64 传出来，中文不受控制台代码页影响） */
function getClipboard() {
  const b64 = execFileSync("powershell.exe", ["-NoProfile", "-Command", "$t = Get-Clipboard -Raw; if ($null -eq $t) { $t = '' }; [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($t))"], { encoding: "utf8" }).trim();
  return Buffer.from(b64, "base64").toString("utf8").replace(/\r?\n$/, "");
}
/** 手机发请求：带上设备 Cookie；POST 带 X-Pierlink 头 */
async function phoneFetch(method, path, body, extra = {}) {
  const headers = { "User-Agent": PHONE_UA, ...extra };
  if (phone.cookie) headers.Cookie = phone.cookie;
  if (method === "POST") { headers["Content-Type"] = "application/json"; headers["X-Pierlink"] = "1"; }
  const res = await fetch(`${phone.base}${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  const text = await res.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch { /* 页面不是 JSON */ }
  return { status: res.status, json, text, headers: res.headers };
}

await rec.step(p, "手机入口：设置里在 127.0.0.1 上打开，生成邀请码", async () => {
  await u.clickText("Settings", "header nav");
  await u.clickText("Phone", ".settings__nav");
  await u.waitFor(`!!document.querySelector('.mobile-set')`, "设置页的手机分区");
  await turnOn("Turn on phone access");
  await u.click(".mobile-set__address .pl-select");
  await u.clickText("127.0.0.1", "[role=listbox]");
  const url = await u.waitFor(`(()=>{const t=document.querySelector('.mobile-set__state')?.innerText||''; const m=t.match(/http:\\/\\/127\\.0\\.0\\.1:\\d+\\//); return m ? m[0] : null;})()`, "手机入口运行中", 20000);
  await u.clickText("Pair a phone", ".mobile-set__pair");
  const code = await u.waitFor(`document.querySelector('.mobile-set__code')?.innerText?.trim() || null`, "邀请码", 10000);
  if (!/^[A-Z2-9]{8}$/.test(code)) throw new Error(`邀请码格式不对：${code}`);
  if (!(await p.js(`!!document.querySelector('.mobile-set__qr-img svg')`))) throw new Error("没有二维码");
  execFileSync("powershell.exe", ["-NoProfile", "-Command", "Set-Clipboard -Value 'empty'"]);
  await u.click('.mobile-set__code-row button[aria-label="Copy code"]');
  await sleep(800);
  if (getClipboard() !== code) throw new Error(`“复制邀请码”没有复制：${getClipboard()}`);
  phone = { base: url.replace(/\/$/, ""), code, cookie: null };
  const page = await phoneFetch("GET", "/");
  if (page.status !== 200 || !page.text.includes('<div id="root">')) throw new Error(`手机页面没出来：${page.status}`);
  if (!page.headers.get("content-security-policy")) throw new Error("手机页面没有 CSP 头");
  return `${url}｜邀请码 ${code}`;
});

await rec.step(p, "手机配对：输入邀请码后电脑上弹出批准框，批准后手机能看状态和终端，没有输入权限时不能输入", async () => {
  if (!phone) throw new Error("手机入口没开");
  const pairing = phoneFetch("POST", "/api/pair", { code: phone.code, device_name: "e2e-phone" });
  await u.waitFor(`/Approve this phone/.test(document.querySelector('.pl-dialog')?.innerText||'')`, "电脑上的批准框", 20000);
  await u.clickText("Approve", ".pl-dialog");
  const paired = await pairing;
  const cookie = paired.headers.get("set-cookie") || "";
  if (paired.status >= 300 || !/pl_device=/.test(cookie) || !/HttpOnly/i.test(cookie)) throw new Error(`配对失败：${paired.status} ${paired.text.slice(0, 200)} ${cookie.slice(0, 40)}`);
  phone.cookie = cookie.split(";")[0];
  // 配对成功：电脑上的二维码自动收起，手机那一行显示系统和浏览器、来源 IP
  await u.waitFor(`!document.querySelector('.mobile-set__qr-img') && /Paired: e2e-phone/.test(document.querySelector('.mobile-set__pair')?.innerText||'')`, "配对成功后二维码收起", 10000);
  const row = await u.waitFor(`[...document.querySelectorAll('.mobile-dev')].find(x=>x.innerText.includes('e2e-phone'))?.innerText || null`, "已配对的手机", 10000);
  if (!/iPhone · Safari/.test(row) || !/127\.0\.0\.1/.test(row)) throw new Error(`手机信息不对：${row.replace(/\s+/g, " ")}`);
  const again = await phoneFetch("POST", "/api/pair", { code: phone.code, device_name: "second" });
  if (again.status < 400) throw new Error("邀请码应该只能用一次");
  const me = await phoneFetch("GET", "/api/me");
  if (!me.json?.permissions?.view || me.json.permissions.input || me.json.permissions.confirm) throw new Error(`新设备默认权限不对：${me.text}`);
  const overview = await phoneFetch("GET", "/api/overview");
  if (!Array.isArray(overview.json?.servers)) throw new Error(`概况不对：${overview.text.slice(0, 200)}`);
  const terms = await phoneFetch("GET", "/api/terminals");
  const local = (terms.json ?? []).find((t) => t.kind === "windows");
  if (!local) throw new Error(`终端列表里没有本机终端：${terms.text.slice(0, 300)}`);
  phone.terminal = local.id;
  const denied = await phoneFetch("POST", `/api/terminals/${encodeURIComponent(local.id)}/input`, { text: "Write-Output nope\r" });
  if (denied.status !== 403) throw new Error(`没有输入权限应该 403：${denied.status}`);
  const foreign = await fetch(`${phone.base}/api/stop-all`, { method: "POST", headers: { Cookie: phone.cookie, "X-Pierlink": "1", Origin: "http://evil.example" } });
  if (foreign.status !== 403) throw new Error(`外来 Origin 应该 403：${foreign.status}`);
  return `设备 ${me.json.name}｜服务器 ${overview.json.servers.length} 台｜终端 ${terms.json.length} 个`;
});

await rec.step(p, "手机改名；关掉二维码后邀请码作废；手机页面的文件按 gzip 发送", async () => {
  if (!phone?.cookie) throw new Error("没有配对好的手机");
  await p.js(`(()=>{const d=[...document.querySelectorAll('.mobile-dev')].find(x=>x.innerText.includes('e2e-phone')); d?.querySelector('button[aria-label="Rename"]')?.click();})()`);
  await u.waitFor(`!!document.querySelector('.mobile-dev__input')`, "改名输入框", 5000);
  await p.js(`(()=>{const i=document.querySelector('.mobile-dev__input'); const set=Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set; set.call(i,'e2e-phone-2'); i.dispatchEvent(new Event('input',{bubbles:true})); i.focus();})()`);
  await u.key("Enter", "Enter", 13, 0, "\r");
  await u.waitFor(`[...document.querySelectorAll('.mobile-dev__name')].some(x=>x.innerText.trim()==='e2e-phone-2')`, "列表里是新名字", 8000);
  const me = await phoneFetch("GET", "/api/me");
  if (me.json?.name !== "e2e-phone-2") throw new Error(`手机那边的名字没变：${me.text.slice(0, 200)}`);
  // 新邀请码：点关闭后作废
  await u.clickText("Pair a phone", ".mobile-set__pair");
  const code = await u.waitFor(`document.querySelector('.mobile-set__code')?.innerText?.trim() || null`, "新的邀请码", 10000);
  await u.click('.mobile-set__pair button[aria-label="Close"]');
  await u.waitFor(`!document.querySelector('.mobile-set__qr-img')`, "点关闭后二维码收起", 5000);
  const saved = phone.cookie;
  phone.cookie = null;
  const late = await phoneFetch("POST", "/api/pair", { code, device_name: "late" });
  phone.cookie = saved;
  if (late.status < 400) throw new Error(`关掉二维码后邀请码还能用：${late.status}`);
  // 网页文件：浏览器接受 gzip 时发压缩好的，不接受时发原文件
  const html = (await phoneFetch("GET", "/")).text;
  const js = html.match(/src="(\/assets\/[^"]+\.js)"/)?.[1];
  if (!js) throw new Error(`首页里找不到脚本：${html.slice(0, 300)}`);
  const zipped = await phoneFetch("GET", js, undefined, { "Accept-Encoding": "gzip" });
  if (zipped.headers.get("content-encoding") !== "gzip" || !/accept-encoding/i.test(zipped.headers.get("vary") || "")) throw new Error(`没有按 gzip 发送：${zipped.headers.get("content-encoding")} / ${zipped.headers.get("vary")}`);
  const plain = await phoneFetch("GET", js, undefined, { "Accept-Encoding": "identity" });
  if (plain.headers.get("content-encoding") || plain.text.length !== zipped.text.length) throw new Error("不接受 gzip 时内容不对");
  return `改名为 e2e-phone-2｜作废的邀请码 ${late.status}｜${js} 按 gzip 发送`;
});

await rec.step(p, "手机输入：电脑上给权限后能在本机终端执行命令；危险命令要手机二次确认；撤销后手机立即失效", async () => {
  if (!phone?.terminal) throw new Error("没有配对好的手机");
  await p.js(`(()=>{const d=[...document.querySelectorAll('.mobile-dev')].find(x=>x.innerText.includes('e2e-phone')); const l=[...(d?.querySelectorAll('label.pl-check')||[])].find(l=>l.textContent.trim().startsWith('Type into terminals')); const i=l?.querySelector('input'); if(i && !i.checked) i.click();})()`);
  await sleep(800);
  const id = encodeURIComponent(phone.terminal);
  const sent = await phoneFetch("POST", `/api/terminals/${id}/input`, { text: 'Write-Output "pl-phone-$(40+2)"\r' });
  if (sent.status >= 300) throw new Error(`输入失败：${sent.status} ${sent.text.slice(0, 200)}`);
  let screen = "";
  for (let i = 0; i < 30 && !/^pl-phone-42/m.test(screen); i++) {
    await sleep(500);
    screen = (await phoneFetch("GET", `/api/terminals/${id}/screen`)).json?.text ?? "";
  }
  if (!/^pl-phone-42/m.test(screen)) throw new Error(`手机上没看到输出：${screen.slice(-300)}`);
  const risky = await phoneFetch("POST", `/api/terminals/${id}/input`, { text: "rm -rf pl-no-such-dir\r" });
  if (risky.status !== 409 || !risky.json?.needs_confirm) throw new Error(`危险命令应该要求确认：${risky.status} ${risky.text.slice(0, 200)}`);
  await u.clickText("Revoke all", ".mobile-set__devices");
  await sleep(800);
  const after = await phoneFetch("GET", "/api/me");
  if (after.status !== 401) throw new Error(`撤销后应该 401：${after.status}`);
  return "执行了 pl-phone-42；危险命令 409；撤销后 401";
});

p.close();
killApp(EXE);
if (wslSshd) {
  try { execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "root", "--", "pkill", "-x", "sshd"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
  wslSshd.kill();
}
process.exitCode = rec.finish() ? 1 : 0;
