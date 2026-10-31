import type { FilesApi } from "./api";
import { baseName, locKey, parentPath } from "./path";
import type { FileEntry, Loc, TransferReportView } from "./types";

/** 预览用的假文件树：路径 → 条目 */
type Tree = Map<string, FileEntry[]>;

const now = Math.floor(Date.now() / 1000);
const f = (name: string, size: number, ago = 3600): FileEntry => ({ name, kind: "file", size, mtime: now - ago, hidden: name.startsWith("."), targetIsDir: null });
const d = (name: string, ago = 7200): FileEntry => ({ name, kind: "dir", size: 0, mtime: now - ago, hidden: name.startsWith("."), targetIsDir: null });

function localTree(): Tree {
  return new Map([
    ["C:\\Users\\me", [d(".ssh"), d("Desktop"), d("Documents"), d("Downloads"), d("code"), f(".gitconfig", 220), f("notes.txt", 1834)]],
    ["C:\\Users\\me\\code", [d("pierlink"), d("ml-exp"), f("todo.md", 512)]],
    ["C:\\Users\\me\\code\\ml-exp", [d("data"), f("train.py", 8_240), f("config.yaml", 913), f("README.md", 2_048)]],
    ["C:\\Users\\me\\Downloads", [f("dataset-v2.zip", 734_003_200, 86400), f("report.pdf", 1_203_334, 172800)]],
  ]);
}
function remoteTree(): Tree {
  const many = Array.from({ length: 40 }, (_, i) => f(`run-${String(i + 1).padStart(3, "0")}.log`, 12_000 + i * 731, i * 600));
  return new Map([
    ["/home/dev", [d(".cache"), d(".config"), d(".ssh"), d("ml-exp"), d("logs"), f(".bashrc", 3_771), f(".profile", 807), f("setup.sh", 1_204)]],
    ["/home/dev/ml-exp", [d("checkpoints"), d("data"), f("train.py", 8_240), f("eval.py", 4_102), f("nohup.out", 92_331)]],
    ["/home/dev/logs", many],
    ["/tmp", [f("x.sock", 0), d("pierlink-test")]],
  ]);
}

/** 浏览器预览：每台机器一棵假文件树，传输会真的往目标树里加条目，并模拟进度 */
export function createPreviewFilesApi(): FilesApi {
  const trees = new Map<string, Tree>();
  const tree = (loc: Loc) => {
    const key = locKey(loc);
    let t = trees.get(key);
    if (!t) { t = loc.kind === "local" ? localTree() : remoteTree(); trees.set(key, t); }
    return t;
  };
  const cancelled = new Set<string>();
  let seq = 0;
  /** 文本内容：机器+路径 → 内容和修改时间 */
  const texts = new Map<string, { text: string; mtime: number }>();
  const entryOf = (loc: Loc, path: string) => tree(loc).get(parentPath(path) ?? "")?.find((e) => e.name === baseName(path));
  const exists = (loc: Loc, path: string) => !!entryOf(loc, path);
  const add = (loc: Loc, path: string, e: FileEntry) => {
    const dir = parentPath(path) ?? "";
    tree(loc).set(dir, [...(tree(loc).get(dir) ?? []), e]);
  };
  const wait = () => new Promise((r) => setTimeout(r, 120));
  const sample = (name: string) => name.endsWith(".py")
    ? "import argparse\n\n\ndef main():\n    print(\"hello\")\n\n\nif __name__ == \"__main__\":\n    main()\n"
    : `# ${name}\n\n预览里的示例内容，可以编辑后按 Ctrl+S 保存。\n`;
  return {
    preview: true,
    async home(loc) { return loc.kind === "local" ? "C:\\Users\\me" : "/home/dev"; },
    async list(loc, path) {
      await new Promise((r) => setTimeout(r, 120));
      const entries = tree(loc).get(path);
      if (!entries) {
        // 没有内容的目录：如果父目录里有它就当空目录，否则报不存在
        const parent = parentPath(path);
        const exists = parent && tree(loc).get(parent)?.some((e) => e.name === baseName(path) && e.kind === "dir");
        if (!exists) throw { code: "fs.not_found", params: { path } };
        return { path, entries: [], truncated: false };
      }
      return { path, entries, truncated: false };
    },
    async terminalCwd() { return "/home/dev/ml-exp"; },
    async plan(req) {
      const target = tree(req.to).get(req.destDir) ?? [];
      const names = req.sources.map(baseName);
      const src = new Map<string, FileEntry>();
      for (const s of req.sources) {
        const e = tree(req.from).get(parentPath(s) ?? "")?.find((x) => x.name === baseName(s));
        if (e) src.set(e.name, e);
      }
      return {
        files: names.length,
        bytes: [...src.values()].reduce((a, e) => a + e.size, 0),
        conflicts: names.filter((n) => target.some((e) => e.name === n)),
      };
    },
    transfer(req, conflict, onProgress) {
      const id = `t${++seq}`;
      const done = (async (): Promise<TransferReportView> => {
        const target = [...(tree(req.to).get(req.destDir) ?? [])];
        const report: TransferReportView = { done: 0, skipped: [], renamed: [], failed: [] };
        const items = req.sources.map((s) => tree(req.from).get(parentPath(s) ?? "")?.find((x) => x.name === baseName(s)) ?? f(baseName(s), 0));
        const total = items.reduce((a, e) => a + Math.max(e.size, 1), 0);
        let doneBytes = 0;
        for (const [i, e] of items.entries()) {
          if (target.some((x) => x.name === e.name) && conflict === "skip") { report.skipped.push(e.name); continue; }
          for (let step = 0; step < 10; step++) {
            await new Promise((r) => setTimeout(r, 80));
            if (cancelled.has(id)) throw { code: "sftp.cancelled", params: {} };
            doneBytes += Math.max(e.size, 1) / 10;
            onProgress({ totalBytes: total, doneBytes: Math.round(doneBytes), current: e.name, filesDone: i, filesTotal: items.length });
          }
          const rest = target.filter((x) => x.name !== e.name);
          target.splice(0, target.length, ...rest, { ...e, mtime: e.mtime });
          report.done++;
        }
        tree(req.to).set(req.destDir, target);
        return report;
      })();
      return { id: Promise.resolve(id), done };
    },
    async cancel(id) { cancelled.add(id); },
    async rename(loc, from, to) {
      await wait();
      if (exists(loc, to)) throw { code: loc.kind === "local" ? "fs.exists" : "sftp.exists", params: { path: to } };
      const e = entryOf(loc, from);
      if (!e) throw { code: "fs.not_found", params: { path: from } };
      const dir = parentPath(from) ?? "";
      tree(loc).set(dir, (tree(loc).get(dir) ?? []).filter((x) => x !== e));
      add(loc, to, { ...e, name: baseName(to), hidden: baseName(to).startsWith(".") });
      const k = `${locKey(loc)}|${from}`;
      if (texts.has(k)) { texts.set(`${locKey(loc)}|${to}`, texts.get(k)!); texts.delete(k); }
    },
    async createDir(loc, path) {
      await wait();
      if (exists(loc, path)) throw { code: "fs.exists", params: { path } };
      add(loc, path, d(baseName(path), 0));
      tree(loc).set(path, []);
    },
    async createFile(loc, path) {
      await wait();
      if (exists(loc, path)) throw { code: "fs.exists", params: { path } };
      add(loc, path, f(baseName(path), 0, 0));
      texts.set(`${locKey(loc)}|${path}`, { text: "", mtime: now });
    },
    async remove(loc, paths) {
      await wait();
      for (const p of paths) {
        const dir = parentPath(p) ?? "";
        tree(loc).set(dir, (tree(loc).get(dir) ?? []).filter((x) => x.name !== baseName(p)));
      }
      return paths.length;
    },
    async readText(loc, path) {
      await wait();
      const e = entryOf(loc, path);
      if (!e) throw { code: "fs.not_found", params: { path } };
      if (/\.(zip|pdf|gz|png)$/.test(e.name)) throw { code: "files.not_text", params: { path } };
      const k = `${locKey(loc)}|${path}`;
      if (!texts.has(k)) texts.set(k, { text: sample(e.name), mtime: e.mtime });
      const t = texts.get(k)!;
      return { text: t.text, bom: false, version: { size: new TextEncoder().encode(t.text).length, mtime: t.mtime, hash: null } };
    },
    async writeText(loc, path, text, _bom, expected) {
      await wait();
      const k = `${locKey(loc)}|${path}`;
      const cur = texts.get(k);
      if (expected && cur && (cur.mtime !== expected.mtime || new TextEncoder().encode(cur.text).length !== expected.size)) throw { code: "files.changed", params: { path } };
      const mtime = Math.max(Math.floor(Date.now() / 1000), (cur?.mtime ?? 0) + 1);
      texts.set(k, { text, mtime });
      const size = new TextEncoder().encode(text).length;
      const dir = parentPath(path) ?? "";
      tree(loc).set(dir, (tree(loc).get(dir) ?? []).map((x) => (x.name === baseName(path) ? { ...x, size, mtime } : x)));
      return { size, mtime, hash: null };
    },
    async mode(loc) { return loc.kind === "local" ? null : 0o644; },
    async chmod() { await wait(); },
    async openDefault() {},
    async reveal() {},
  };
}

