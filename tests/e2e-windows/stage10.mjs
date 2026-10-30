// MCP 逐个工具检查的 Windows 端到端测试：像 AI 助手一样把 20 个工具里 stage8 没覆盖到的都用一遍
// （改几行、搜索、找文件、后台任务、终端读屏），服务器入口上的文件工具（服务器上的 AI 操作 Windows 文件），
// 以及“一键添加”到真实安装的 Claude Code、Codex：用它们自己的命令检查配置能读、能连上。
// 在 stage8.mjs 之后运行：那里已经打开了 AI 接入、本机入口（总是允许）和 pl-linux 的服务器入口。
//
// 环境变量：PIERLINK_EXE、E2E_OUT（必填）；PIERLINK_TEST_LINUX_*（同 stage6.mjs，没有时跳过服务器相关步骤）
// 每一步截图；某一步失败不中断后面的步骤。全部通过时退出码为 0。
import net from "node:net";
import { join } from "node:path";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, openSync, readFileSync, writeFileSync } from "node:fs";
import { killApp, launchApp, recorder, sleep } from "./lib.mjs";

const EXE = process.env.PIERLINK_EXE;
const OUT = process.env.E2E_OUT && join(process.env.E2E_OUT, "0.9.4-mcp");
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
/** 这一阶段给 AI 授权的文件夹（路径里带空格和中文，和用户的真实目录一样） */
const DEEP = join(OUT, "项目 deep");
mkdirSync(DEEP, { recursive: true });

const rec = recorder(OUT);
killApp(EXE);
await sleep(1000);
const { p, u } = await launchApp(EXE, OUT, PORT);

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

/** 按 MCP stdio 方式说话的客户端：启动 `Pierlink.exe --mcp`，一行一个 JSON（同 stage8） */
class Agent {
  constructor() {
    this.proc = spawn(EXE, ["--mcp"], { stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
    this.buf = "";
    this.stderr = "";
    this.next = 0;
    this.waiting = new Map();
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
      for (const done of this.waiting.values()) done({ error: { message: `桥接程序已退出（${code}）` } });
      this.waiting.clear();
    });
  }
  request(method, params = {}, timeoutMs = 120000) {
    const id = ++this.next;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.waiting.delete(id); reject(new Error(`等待 ${method} 超时；桥接程序输出：${this.stderr.slice(-500)}`)); }, timeoutMs);
      this.waiting.set(id, (msg) => { clearTimeout(timer); resolve(msg); });
      this.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    });
  }
  notify(method, params = {}) { this.proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, params })}\n`); }
  async initialize() {
    const r = await this.request("initialize", { protocolVersion: "2025-11-25", capabilities: {}, clientInfo: { name: "e2e 逐个工具检查", version: "1" } });
    if (r.error) throw new Error(`握手失败：${JSON.stringify(r.error)}`);
    this.notify("notifications/initialized");
  }
  close() { try { this.proc.stdin.end(); } catch { /* 已退出 */ } }
}

/** 调用工具；期间弹出 AI 确认框时点 label（默认“允许这一次”） */
async function call(agent, name, args = {}, label = "Allow this time", timeoutMs = 120000) {
  let done = false;
  let result;
  agent.request("tools/call", { name, arguments: args }, timeoutMs).then((r) => { done = true; result = r; }, (e) => { done = true; result = { error: { message: String(e) } }; });
  const end = Date.now() + timeoutMs + 5000;
  while (!done && Date.now() < end) {
    if (await p.js(`!!document.querySelector('.pl-dialog .mcp-confirm')`).catch(() => false)) {
      await u.clickText(label, ".pl-dialog");
      await sleep(500);
    }
    await sleep(300);
  }
  return result;
}
/** 调用工具，期间不该弹确认框：弹了就点“拒绝”并报错 */
async function callNoDialog(agent, name, args = {}, timeoutMs = 120000) {
  let done = false;
  let result;
  let dialog = null;
  agent.request("tools/call", { name, arguments: args }, timeoutMs).then((r) => { done = true; result = r; }, (e) => { done = true; result = { error: { message: String(e) } }; });
  const end = Date.now() + timeoutMs + 5000;
  while (!done && Date.now() < end) {
    const text = await p.js(`(()=>{const d=document.querySelector('.pl-dialog .mcp-confirm')?.closest('.pl-dialog'); return d ? d.innerText.replace(/\\s+/g,' ') : null;})()`).catch(() => null);
    if (text) {
      dialog = text;
      await u.clickText("Deny", ".pl-dialog");
      await sleep(500);
    }
    await sleep(300);
  }
  if (dialog) throw new Error(`${name} 不该弹确认框：${dialog.slice(0, 200)}`);
  return result;
}
/** 工具结果里的结构化内容；工具报错时抛出它给模型看的文字 */
function ok(r) {
  if (r.error) throw new Error(`请求出错：${JSON.stringify(r.error)}`);
  if (r.result?.isError) throw new Error(`工具返回错误：${r.result.content?.[0]?.text}`);
  return r.result.structuredContent;
}
/** 工具报错时给模型看的文字；没报错时抛出 */
function failed(r) {
  if (!r.result?.isError) throw new Error(`应该报错却成功了：${JSON.stringify(r).slice(0, 300)}`);
  return r.result.content?.[0]?.text ?? "";
}

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
/** 在左边的入口列表里选中文字含 text 的一行 */
async function selectEntry(text) {
  const found = await p.js(`(()=>{const r=[...document.querySelectorAll('.mcp-row')].find(r=>r.innerText.includes(${JSON.stringify(text)})); if(!r) return false; r.click(); return true;})()`);
  if (!found) throw new Error(`入口列表里没有：${text}`);
  await sleep(600);
}
async function check(label) {
  const done = await p.js(`(()=>{const l=[...document.querySelectorAll('.mcp-detail label.pl-check')].find(l=>l.textContent.trim()===${JSON.stringify(label)}); const i=l?.querySelector('input'); if(!i) return null; if(!i.checked) i.click(); return true;})()`);
  if (!done) throw new Error(`找不到复选框：${label}`);
  await sleep(500);
}
async function addFolder(folder) {
  if (await p.js(`[...document.querySelectorAll('.mcp-detail .mcp-perms__folder')].some(f=>f.innerText.includes(${JSON.stringify(folder)}))`)) return;
  const found = await p.js(`(()=>{const i=document.querySelector('.mcp-detail .mcp-perms__add input'); if(!i) return false; i.focus(); i.select(); return true;})()`);
  if (!found) throw new Error("找不到文件夹输入框");
  await u.type(folder);
  await u.clickText("Add", ".mcp-detail .mcp-perms__add");
  await u.waitFor(`[...document.querySelectorAll('.mcp-detail .mcp-perms__folder')].some(f=>f.innerText.includes(${JSON.stringify(folder)}))`, "文件夹出现在列表里");
}
/** 在 WSL 里以测试用户运行 python 脚本（-e 直接执行，不经过 bash），返回退出码和输出 */
function wslPython(script, args = [], timeout = 180000) {
  const r = spawnSync("wsl.exe", ["-d", LX.wsl, "-u", LX.user, "-e", "python3", "-c", script, ...args], { encoding: "utf8", timeout });
  return { code: r.status, out: r.stdout ?? "", err: r.stderr ?? "" };
}

await rec.step(p, "准备：本机入口和 pl-linux 的服务器入口都授权一个带空格和中文的文件夹", async () => {
  await openMcpPage();
  await turnOn("Turn on AI access");
  await selectEntry("This PC");
  await turnOn("Local entry (AI on this PC)");
  await addFolder(DEEP);
  await check("Allow running commands on this PC");
  if (HAVE_LINUX) {
    await check(LX_NAME);
    // 服务器入口在 stage8 里加过；这里给它也授权这个文件夹，服务器上的 AI 才能操作里面的文件
    await selectEntry(LX_NAME);
    await check("Allow running commands on this PC");
    await addFolder(DEEP);
  }
  return DEEP;
});

let agent = null;
const CRLF_FILE = join(DEEP, "说明 notes.txt");
await rec.step(p, "read_file：中文和 Windows 换行的文件按行读，分页，带编码、换行和 sha256", async () => {
  agent = new Agent();
  await agent.initialize();
  const lines = Array.from({ length: 30 }, (_, i) => `第 ${i + 1} 行：内容 line ${i + 1}`);
  writeFileSync(CRLF_FILE, Buffer.from(`${lines.join("\r\n")}\r\n`, "utf8"));
  const r = ok(await call(agent, "read_file", { file_path: CRLF_FILE, offset: 10, limit: 5 }));
  const text = JSON.stringify(r);
  if (!r.content?.includes("第 10 行") || !r.content.includes("第 14 行") || r.content.includes("第 15 行")) throw new Error(`分页内容不对：${text.slice(0, 400)}`);
  if (r.total_lines !== 30 || !r.sha256 || !/crlf/i.test(String(r.line_ending))) throw new Error(`元数据不对：${text.slice(0, 400)}`);
  return `编码 ${r.encoding}，换行 ${r.line_ending}，共 ${r.total_lines} 行`;
});

await rec.step(p, "edit_file：多行替换保持 Windows 换行；不唯一时报错；replace_all；sha256 不符时不改", async () => {
  if (!agent) throw new Error("没有客户端");
  // AI 从 read_file 看到的是去掉 \r 的文字，多行 old_string 用 \n
  ok(await call(agent, "edit_file", { file_path: CRLF_FILE, old_string: "第 3 行：内容 line 3\n第 4 行：内容 line 4", new_string: "第 3 行：改过\n新增一行\n第 4 行：改过" }));
  const bytes = readFileSync(CRLF_FILE);
  const text = bytes.toString("utf8");
  if (!text.includes("第 3 行：改过\r\n新增一行\r\n第 4 行：改过\r\n")) throw new Error(`多行替换后的内容或换行不对：${JSON.stringify(text.slice(0, 200))}`);
  if (/[^\r]\n/.test(text)) throw new Error("文件里出现了单独的 \\n，换行被改坏了");
  const notUnique = failed(await call(agent, "edit_file", { file_path: CRLF_FILE, old_string: "内容 line", new_string: "x" }));
  const all = ok(await call(agent, "edit_file", { file_path: CRLF_FILE, old_string: "内容 line", new_string: "内容 LINE", replace_all: true }));
  if (readFileSync(CRLF_FILE, "utf8").includes("内容 line")) throw new Error("replace_all 没有全部替换");
  const before = readFileSync(CRLF_FILE);
  const stale = failed(await call(agent, "edit_file", { file_path: CRLF_FILE, old_string: "新增一行", new_string: "不该写进去", expected_sha256: "0".repeat(64) }));
  if (!readFileSync(CRLF_FILE).equals(before)) throw new Error("sha256 不符时文件还是被改了");
  return `不唯一：${notUnique.slice(0, 80)}｜全部替换：${JSON.stringify(all).slice(0, 80)}｜sha256 不符：${stale.slice(0, 80)}`;
});

await rec.step(p, "write_file：新建文件；覆盖已有的 Windows 换行文件时保持 \\r\\n", async () => {
  if (!agent) throw new Error("没有客户端");
  const created = join(DEEP, "新文件.md");
  ok(await call(agent, "write_file", { file_path: created, content: "# 标题\n内容\n" }));
  if (readFileSync(created, "utf8") !== "# 标题\n内容\n") throw new Error(`新建的内容不对：${JSON.stringify(readFileSync(created, "utf8"))}`);
  ok(await call(agent, "write_file", { file_path: CRLF_FILE, content: "整个重写\n第二行\n" }));
  const text = readFileSync(CRLF_FILE, "utf8");
  if (text !== "整个重写\r\n第二行\r\n") throw new Error(`覆盖后没保持 \\r\\n：${JSON.stringify(text)}`);
  return "新建和覆盖都对";
});

await rec.step(p, "search：正则搜索带行号和上下文；按文件、计数；glob 过滤；遵守 .gitignore", async () => {
  if (!agent) throw new Error("没有客户端");
  const src = join(DEEP, "src");
  mkdirSync(join(src, "sub"), { recursive: true });
  mkdirSync(join(DEEP, "node_modules", "lib"), { recursive: true });
  writeFileSync(join(DEEP, ".gitignore"), "node_modules/\n");
  writeFileSync(join(src, "app.ts"), "const a = 1;\n// TODO(pl): 第一处\nconst b = 2;\n");
  writeFileSync(join(src, "sub", "tool.py"), "x = 1\n# TODO(pl): 第二处\n");
  writeFileSync(join(DEEP, "node_modules", "lib", "dep.js"), "// TODO(pl): 依赖里的\n");
  const content = ok(await call(agent, "search", { pattern: "TODO\\(pl\\)", path: DEEP, output_mode: "content", "-C": 1 }));
  const hits = content.results.filter((x) => x.match);
  if (hits.length !== 2 || hits.some((x) => x.path.includes("node_modules"))) throw new Error(`匹配不对（应该 2 处、不含 node_modules）：${JSON.stringify(content).slice(0, 500)}`);
  if (!hits.every((x) => typeof x.line_number === "number" && x.line_number === 2)) throw new Error(`行号不对：${JSON.stringify(hits)}`);
  if (content.results.length < 4) throw new Error(`没带上下文行：${JSON.stringify(content.results).slice(0, 300)}`);
  const files = ok(await call(agent, "search", { pattern: "TODO\\(pl\\)", path: DEEP }));
  const count = ok(await call(agent, "search", { pattern: "TODO\\(pl\\)", path: DEEP, output_mode: "count", glob: "**/*.py" }));
  if (files.results.length !== 2) throw new Error(`files_with_matches 不对：${JSON.stringify(files)}`);
  if (count.results.length !== 1 || !count.results[0].path.endsWith("tool.py") || count.results[0].count !== 1) throw new Error(`glob 加计数不对：${JSON.stringify(count)}`);
  const bad = failed(await call(agent, "search", { pattern: "([", path: DEEP }));
  return `2 处匹配，含上下文 ${content.results.length} 行｜坏正则：${bad.slice(0, 80)}`;
});

await rec.step(p, "find_files 和 list_dir：按 glob 找、新的在前；include_ignored 才看得到忽略的文件", async () => {
  if (!agent) throw new Error("没有客户端");
  await sleep(1100);
  writeFileSync(join(DEEP, "src", "newest.ts"), "export {};\n");
  const found = ok(await call(agent, "find_files", { pattern: "**/*.ts", path: DEEP }));
  const paths = found.paths.map((x) => (typeof x === "string" ? x : x.path));
  if (paths.length !== 2 || !paths[0].endsWith("newest.ts")) throw new Error(`找文件不对（应该 2 个、最新的在前）：${JSON.stringify(found)}`);
  const ignored = ok(await call(agent, "find_files", { pattern: "**/*.js", path: DEEP }));
  const withIgnored = ok(await call(agent, "find_files", { pattern: "**/*.js", path: DEEP, include_ignored: true }));
  if (ignored.paths.length !== 0 || withIgnored.paths.length !== 1) throw new Error(`忽略规则不对：${JSON.stringify(ignored)} / ${JSON.stringify(withIgnored)}`);
  const list = ok(await call(agent, "list_dir", { path: DEEP }));
  const text = JSON.stringify(list);
  if (!text.includes("新文件.md") || !text.includes("src")) throw new Error(`列目录不对：${text.slice(0, 300)}`);
  return `找到 ${paths.length} 个 .ts；列目录 ${text.length} 字`;
});

await rec.step(p, "后台任务：background 立即返回 job_id；job_output 读到输出和退出码；list_jobs；job_stop 结束长任务", async () => {
  if (!agent) throw new Error("没有客户端");
  const started = Date.now();
  const job = ok(await call(agent, "run_command", { target: "windows", command: "1..5 | ForEach-Object { Write-Output \"tick $_\"; Start-Sleep -Milliseconds 300 }", background: true }));
  if (!job.job_id || Date.now() - started > 15000) throw new Error(`没有立即返回 job_id：${JSON.stringify(job)}`);
  let out = null;
  let stdout = "";
  for (let i = 0; i < 60; i++) {
    out = ok(await call(agent, "job_output", { job_id: job.job_id, since: 0 }));
    stdout = out.stdout;
    if (!out.running) break;
    await sleep(500);
  }
  if (out.running || out.exit_code !== 0 || !stdout.includes("tick 5")) throw new Error(`后台任务结果不对：${JSON.stringify(out).slice(0, 300)}`);
  const long = ok(await call(agent, "run_command", { target: "windows", command: "Start-Sleep -Seconds 120", background: true }));
  const jobs = ok(await call(agent, "list_jobs"));
  if (!JSON.stringify(jobs).includes(long.job_id)) throw new Error(`list_jobs 里没有长任务：${JSON.stringify(jobs).slice(0, 300)}`);
  ok(await call(agent, "job_stop", { job_id: long.job_id }));
  let stopped = null;
  for (let i = 0; i < 30; i++) {
    stopped = ok(await call(agent, "job_output", { job_id: long.job_id }));
    if (!stopped.running) break;
    await sleep(500);
  }
  if (stopped.running) throw new Error("job_stop 之后任务还在运行");
  return `短任务输出 ${stdout.trim().split(/\r?\n/).length} 行；长任务已停止`;
});

await rec.step(p, "终端：AI 终端里 Ctrl+C 中断、terminal_read 按游标读、terminal_screen 读屏、close_terminal 关闭", async () => {
  if (!agent) throw new Error("没有客户端");
  if (!HAVE_LINUX) throw new Error("没有 WSL 里的 Linux 服务器");
  await u.clickText("Workspace", "header nav");
  const opened = ok(await call(agent, "open_terminal", { target: LX_NAME, cwd: "/tmp" }, "Allow this time", 150000));
  const id = opened.terminal_id;
  ok(await call(agent, "terminal_wait", { terminal_id: id, quiet_ms: 1500, timeout_secs: 30 }));
  const first = ok(await call(agent, "terminal_read", { terminal_id: id, since: 0 }));
  ok(await call(agent, "terminal_send", { terminal_id: id, text: "sleep 100", keys: ["Enter"] }));
  await sleep(1500);
  ok(await call(agent, "terminal_send", { terminal_id: id, keys: ["Ctrl+C"] }));
  ok(await call(agent, "terminal_send", { terminal_id: id, text: "echo pl-after-$((6*7))", keys: ["Enter"] }));
  const waited = ok(await call(agent, "terminal_wait", { terminal_id: id, pattern: "pl-after-42", since: first.cursor, timeout_secs: 30 }));
  if (!waited.matched) throw new Error(`Ctrl+C 之后没等到下一条命令的输出：${JSON.stringify(waited).slice(0, 300)}`);
  const more = ok(await call(agent, "terminal_read", { terminal_id: id, since: first.cursor }));
  if (!more.text.includes("pl-after-42") || more.cursor <= first.cursor) throw new Error(`按游标读不对：${JSON.stringify(more).slice(0, 300)}`);
  const screen = ok(await call(agent, "terminal_screen", { terminal_id: id }));
  if (!JSON.stringify(screen).includes("pl-after-42")) throw new Error(`读屏里没有最新输出：${JSON.stringify(screen).slice(0, 300)}`);
  ok(await call(agent, "close_terminal", { terminal_id: id }));
  const listed = ok(await call(agent, "list_terminals"));
  if (JSON.stringify(listed.ai_terminals ?? listed).includes(id)) throw new Error("关闭后 list_terminals 里还有这个终端");
  return `终端 ${id.slice(0, 8)}… 已关闭`;
});

await rec.step(p, "Windows AI 终端：换行结尾的命令会执行；第一条命令不弹“内容未知”；send 返回的 cursor 直接给 wait 用", async () => {
  if (!agent) throw new Error("没有客户端");
  await u.clickText("Workspace", "header nav");
  const opened = ok(await call(agent, "open_terminal", { target: "windows" }, "Allow this time", 150000));
  const id = opened.terminal_id;
  ok(await call(agent, "terminal_wait", { terminal_id: id, quiet_ms: 2000, timeout_secs: 30 }));
  const sent = ok(await callNoDialog(agent, "terminal_send", { terminal_id: id, text: "Write-Output \"pl-lf-$(6*7)\"\n" }));
  if (typeof sent.cursor !== "number") throw new Error(`terminal_send 没有返回 cursor：${JSON.stringify(sent)}`);
  // ^ 按行匹配（多行模式）：只匹配输出那一行，不匹配回显的命令
  const waited = ok(await call(agent, "terminal_wait", { terminal_id: id, pattern: "^pl-lf-42", since: sent.cursor, timeout_secs: 30 }));
  if (!waited.matched) throw new Error(`换行结尾的命令没有执行：${JSON.stringify(waited).slice(0, 300)}`);
  ok(await call(agent, "close_terminal", { terminal_id: id }));
  return `终端 ${id.slice(0, 8)}…：命令已执行`;
});

await rec.step(p, "run_command：Windows 上原生程序的退出码原样返回；最后一条成功时是 0", async () => {
  if (!agent) throw new Error("没有客户端");
  const failedRun = ok(await call(agent, "run_command", { target: "windows", command: "cmd /c exit 3" }));
  if (failedRun.exit_code !== 3) throw new Error(`cmd /c exit 3 的退出码应该是 3：${JSON.stringify(failedRun).slice(0, 200)}`);
  const later = ok(await call(agent, "run_command", { target: "windows", command: "cmd /c exit 3; Write-Output after" }));
  if (later.exit_code !== 0) throw new Error(`最后一条命令成功时退出码应该是 0：${JSON.stringify(later).slice(0, 200)}`);
  return "3 和 0 都对";
});

await rec.step(p, "授权文件夹里改 .js、.py 不弹确认框；写 .ps1 仍要确认（点拒绝后没写）", async () => {
  if (!agent) throw new Error("没有客户端");
  const js = join(DEEP, "src", "index.js");
  ok(await callNoDialog(agent, "write_file", { file_path: js, content: "console.log(1);\n" }));
  ok(await callNoDialog(agent, "edit_file", { file_path: js, old_string: "console.log(1)", new_string: "console.log(2)" }));
  ok(await callNoDialog(agent, "write_file", { file_path: join(DEEP, "tool2.py"), content: "print(1)\n" }));
  const ps1 = join(DEEP, "run.ps1");
  const denied = failed(await call(agent, "write_file", { file_path: ps1, content: "Write-Output 1\n" }, "Deny"));
  if (existsSync(ps1)) throw new Error("拒绝了却还是写了 .ps1");
  return `.ps1：${denied.slice(0, 80)}`;
});

if (agent) agent.close();

await rec.step(p, "服务器入口：服务器上的 AI 读、改几行、搜索、找 Windows 上的文件", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL 里的 Linux 服务器");
  const file = join(DEEP, "server-edit.txt");
  writeFileSync(file, Buffer.from("alpha\r\nbeta\r\ngamma\r\n", "utf8"));
  const script = [
    "import json, os, sys, urllib.request",
    "folder, file = sys.argv[1], sys.argv[2]",
    "cfg = json.load(open(os.path.expanduser('~/.pierlink/mcp.json')))",
    "h = {'Content-Type': 'application/json', 'Accept': 'application/json, text/event-stream', 'Authorization': 'Bearer ' + cfg['token']}",
    "def post(body, extra={}):",
    "    req = urllib.request.Request(cfg['url'], data=json.dumps(body).encode(), headers={**h, **extra}, method='POST')",
    "    with urllib.request.urlopen(req, timeout=150) as r:",
    "        raw = r.read().decode()",
    "        sid = r.headers.get('Mcp-Session-Id')",
    "    if raw.startswith('event:') or raw.startswith('data:'):",
    "        raw = [l[5:].strip() for l in raw.splitlines() if l.startswith('data:')][-1]",
    "    return sid, (json.loads(raw) if raw.strip() else None)",
    "sid, _ = post({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {'protocolVersion': '2025-11-25', 'capabilities': {}, 'clientInfo': {'name': 'wsl-agent', 'version': '1'}}})",
    "extra = {'MCP-Protocol-Version': '2025-11-25', **({'Mcp-Session-Id': sid} if sid else {})}",
    "post({'jsonrpc': '2.0', 'method': 'notifications/initialized'}, extra)",
    "n = [10]",
    "def call(name, args):",
    "    n[0] += 1",
    "    _, r = post({'jsonrpc': '2.0', 'id': n[0], 'method': 'tools/call', 'params': {'name': name, 'arguments': args}}, extra)",
    "    res = r.get('result', {})",
    "    if r.get('error') or res.get('isError'):",
    "        print('FAIL', name, json.dumps(r, ensure_ascii=False)[:400]); sys.exit(1)",
    "    return res.get('structuredContent')",
    "read = call('read_file', {'file_path': file})",
    "assert 'beta' in read['content'], read",
    "call('edit_file', {'file_path': file, 'old_string': 'beta\\ngamma', 'new_string': 'BETA\\nGAMMA'})",
    "found = call('search', {'pattern': 'GAMMA', 'path': folder, 'output_mode': 'content'})",
    "assert any(x.get('match') for x in found['results']), found",
    "files = call('find_files', {'pattern': '**/server-edit.txt', 'path': folder})",
    "assert len(files['paths']) == 1, files",
    "print('OK', read['line_ending'], len(found['results']))",
  ].join("\n");
  // 服务器入口这次会话第一次调用工具时要确认：在后台跑 WSL 里的客户端，这边答确认框
  const child = spawn("wsl.exe", ["-d", LX.wsl, "-u", LX.user, "-e", "python3", "-c", script, DEEP, file], { stdio: ["ignore", "pipe", "pipe"] });
  let out = "";
  let err = "";
  child.stdout.on("data", (d) => { out += d; });
  child.stderr.on("data", (d) => { err += d; });
  let code = null;
  child.on("exit", (c) => { code = c; });
  const end = Date.now() + 180000;
  while (code === null && Date.now() < end) {
    if (await p.js(`!!document.querySelector('.pl-dialog .mcp-confirm')`).catch(() => false)) {
      await u.clickText("Always allow", ".pl-dialog");
      await sleep(500);
    }
    await sleep(400);
  }
  if (code !== 0) throw new Error(`服务器上的 AI 调用失败（${code}）：${out.slice(-500)}｜${err.slice(-500)}`);
  const text = readFileSync(file, "utf8");
  if (text !== "alpha\r\nBETA\r\nGAMMA\r\n") throw new Error(`服务器上的 AI 改出来的文件不对：${JSON.stringify(text)}`);
  return out.trim();
});

/** 在 Windows 上运行一个命令行客户端（npm 全局装的 .cmd 要经 cmd.exe） */
function windowsCli(command) {
  const r = spawnSync("cmd.exe", ["/d", "/s", "/c", command], { encoding: "utf8", timeout: 120000, windowsHide: true });
  return { code: r.status, out: `${r.stdout ?? ""}${r.stderr ?? ""}` };
}
/** 点 MCP 页上的按钮（文字完全等于 label），等出现这个客户端的结果提示 */
async function clickAdd(label, client) {
  const found = await p.js(`(()=>{const b=[...document.querySelectorAll('.mcp-detail button')].find(b=>b.textContent.trim()===${JSON.stringify(label)}); if(!b) return false; if(b.disabled) return 'disabled'; b.click(); return true;})()`);
  if (found !== true) return found === "disabled" ? "按钮是灰的" : "没有这个按钮";
  const re = `(Added to ${client}|Couldn't add to ${client}|${client} wasn't found)[^\\n]*`;
  return await u.waitFor(`(()=>{const m=(document.querySelector('.mcp-detail')?.innerText||'').match(new RegExp(${JSON.stringify(re)})); return m ? m[0] : null;})()`, `点“${label}”后的结果`, 120000);
}

await rec.step(p, "一键添加到本机 Claude Code、Codex：它们自己的命令能看到 pierlink 并连上", async () => {
  const have = (cmd) => windowsCli(`where ${cmd}`).code === 0;
  const clients = [["claude", "Claude Code"], ["codex", "Codex"]].filter(([cmd]) => have(cmd));
  if (!clients.length) return "跳过：这台机器上没装 Claude Code、Codex";
  await openMcpPage();
  await selectEntry("This PC");
  // 页面打开时检测客户端；装好客户端后要重新打开页面
  await u.clickText("Workspace", "header nav");
  await openMcpPage();
  await selectEntry("This PC");
  const results = [];
  for (const [cmd, name] of clients) {
    const shown = await clickAdd(`Add to ${name}`, name);
    if (!/Added to/.test(shown)) throw new Error(`点“Add to ${name}”的结果不对：${shown}`);
    const listed = windowsCli(`${cmd} mcp list`);
    if (listed.code !== 0 || !/pierlink/i.test(listed.out)) throw new Error(`${cmd} mcp list 里没有 pierlink（${listed.code}）：${listed.out.slice(-600)}`);
    // 能连上的客户端会把它标成已连接（Claude Code：✓ Connected）；只记录，连接检查的输出各版本写法不同
    results.push(`${name}：${listed.out.split(/\r?\n/).find((l) => /pierlink/i.test(l))?.trim()}`);
    if (cmd === "codex") {
      const config = readFileSync(join(process.env.USERPROFILE, ".codex", "config.toml"), "utf8");
      if (!/\[mcp_servers\.pierlink\][^[]*tool_timeout_sec = 1800/.test(config)) throw new Error(`本机 Codex 配置里没有 30 分钟的工具超时：${config.slice(-400)}`);
    }
  }
  return results.join("｜");
});

await rec.step(p, "一键添加到服务器上的 Claude Code、Codex：服务器上的客户端读到配置并连上服务器入口", async () => {
  if (!HAVE_LINUX) throw new Error("没有 WSL 里的 Linux 服务器");
  const has = (cmd) => wslPython(`import shutil, sys; sys.exit(0 if shutil.which(${JSON.stringify(cmd)}) else 1)`).code === 0;
  const clients = [["claude", "Claude Code"], ["codex", "Codex"]].filter(([cmd]) => has(cmd));
  if (!clients.length) return "跳过：WSL 里没装 Claude Code、Codex";
  await u.clickText("Workspace", "header nav");
  await openMcpPage();
  await selectEntry(LX_NAME);
  const results = [];
  for (const [cmd, name] of clients) {
    const label = name === "Claude Code" ? "Add to Claude Code on the server" : `Add to ${name} on the server`;
    const shown = await clickAdd(label, name);
    if (!/Added to/.test(shown)) throw new Error(`点“${label}”的结果不对：${shown}`);
    const r = wslPython(`import subprocess, shutil; r = subprocess.run([shutil.which(${JSON.stringify(cmd)}), 'mcp', 'list'], capture_output=True, text=True, timeout=120); print(r.returncode); print(r.stdout + r.stderr)`);
    const [rc, ...rest] = r.out.split("\n");
    const text = rest.join("\n");
    if (rc.trim() !== "0" || !/pierlink/i.test(text)) throw new Error(`服务器上 ${cmd} mcp list 里没有 pierlink（${rc}）：${text.slice(-600)}｜${r.err.slice(-300)}`);
    if (cmd === "codex") {
      const toml = wslPython("import os; print(open(os.path.expanduser('~/.codex/config.toml')).read())");
      if (!/\[mcp_servers\.pierlink\][^[]*tool_timeout_sec = 1800/.test(toml.out)) throw new Error(`服务器上的 Codex 配置里没有 30 分钟的工具超时：${toml.out.slice(-400)}`);
    }
    // 令牌会不会被客户端自己的列表命令打印出来，只记录（那是客户端的行为）
    const leaked = /Bearer\s+[A-Za-z0-9_-]{8,}/.test(text) ? "（列表里显示了令牌）" : "";
    results.push(`${name}：${text.split("\n").find((l) => /pierlink/i.test(l))?.trim()}${leaked}`);
  }
  return results.join("｜");
});

p.close();
killApp(EXE);
if (wslSshd) {
  try { execFileSync("wsl.exe", ["-d", LX.wsl, "-u", "root", "--", "pkill", "-x", "sshd"], { stdio: "ignore" }); } catch { /* 已经退出 */ }
  wslSshd.kill();
}
process.exitCode = rec.finish() ? 1 : 0;
