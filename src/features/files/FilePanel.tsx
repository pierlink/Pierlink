import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from "react";
import { useTranslation } from "react-i18next";
import { ArrowDownToLine, ArrowUpFromLine, ChevronRight, Eye, EyeOff, File, FilePlus, Folder, FolderPlus, FolderSymlink, Link2, LocateFixed, RefreshCw, X } from "lucide-react";
import { useContextMenu, type MenuEntry } from "../../components";
import { formatError } from "../../errors";
import type { OpenSpec } from "../terminal/types";
import { getFilesApi } from "./api";
import { beginDrag, registerDropTarget, useDragState, type DragPayload } from "./dnd";
import { baseName, crumbs, formatSize, joinPath, locKey, parentPath } from "./path";
import { requestDrop } from "./TransferCenter";
import { TextEditor } from "./TextEditor";
import { isBinaryName } from "./fileTypes";
import { ChmodDialog, DeleteDialog } from "./FileDialogs";
import { onDirChanged, useTransfers } from "./transfers";
import type { FileEntry, Listing, Loc } from "./types";
import "./files.css";
import { writeClipboardText } from "../shell/clipboard";

/** 终端的打开方式 → 文件所在的机器 */
function locOf(spec: OpenSpec): Loc {
  return spec.kind === "local" ? { kind: "local" } : spec;
}

/** 错误文字。浏览文件要另外建立一条 SSH 连接：用密码登录且没保存密码时无法自动登录，附上提示 */
function describeError(e: unknown, authHint: string): string {
  const code = (e as { code?: string } | null)?.code ?? "";
  return code === "ssh.auth_failed" || code.startsWith("ssh.askpass") ? `${formatError(e)}。${authHint}` : formatError(e);
}

/** 目录排在前面，再按名字（不区分大小写）排序 */
function sortEntries(entries: FileEntry[]): FileEntry[] {
  const isDir = (e: FileEntry) => e.kind === "dir" || (e.kind === "symlink" && e.targetIsDir === true);
  return [...entries].sort((a, b) => Number(isDir(b)) - Number(isDir(a)) || a.name.localeCompare(b.name, undefined, { sensitivity: "base", numeric: true }));
}

/** 修改时间：今天显示时分，今年显示月日，更早显示年月日 */
/** 每个文件面板最多记住多少个目录的列表 */
const CACHE_MAX = 100;

function formatTime(sec: number): string {
  if (!sec) return "";
  const d = new Date(sec * 1000);
  const now = new Date();
  const pad = (n: number) => String(n).padStart(2, "0");
  if (d.toDateString() === now.toDateString()) return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  if (d.getFullYear() === now.getFullYear()) return `${d.getMonth() + 1}-${pad(d.getDate())}`;
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/**
 * 文件面板：显示终端当前目录的文件（含隐藏文件），终端里 cd 后跟着切换。
 * 手动进入别的目录后暂停跟随，点顶部的“跟随终端目录”按钮恢复。可以把选中的文件拖到另一个窗格的文件面板上传输。
 */
export function FilePanel({ paneId, spec, title, cwd, commands, sessionId, onClose }: {
  paneId: string;
  spec: OpenSpec;
  /** 窗格标题（拖放提示里显示目标） */
  title: string;
  /** 本机终端报告的当前目录 */
  cwd: string | null;
  /** 终端里按回车的次数（服务器终端据此重新查询当前目录） */
  commands: number;
  /** 终端会话 ID（服务器终端查询当前目录用；未连接时为 null） */
  sessionId: string | null;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const loc = useMemo(() => locOf(spec), [spec]);
  const [home, setHome] = useState<string | null>(null);
  const [listing, setListing] = useState<Listing | null>(null);
  const [path, setPath] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [follow, setFollow] = useState(true);
  /** 服务器终端不支持查询当前目录（没有 /proc 或不接受环境变量） */
  const [noFollow, setNoFollow] = useState(false);
  const [termDir, setTermDir] = useState<string | null>(null);
  const [showHidden, setShowHidden] = useState(true);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const anchor = useRef<string | null>(null);
  const seq = useRef(0);
  const ctx = useContextMenu(t("files.menuLabel"));
  const drag = useDragState();
  /** 正在原地重命名的项（名字） */
  const [renaming, setRenaming] = useState<string | null>(null);
  /** 正在新建：在列表最前面出现一个输入框 */
  const [creating, setCreating] = useState<"dir" | "file" | null>(null);
  /** 正在软件内编辑的文件（完整路径） */
  const [editing, setEditing] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string[] | null>(null);
  const [chmodPath, setChmodPath] = useState<string | null>(null);
  /** 操作失败的提示（几秒后消失） */
  const [opError, setOpError] = useState<string | null>(null);
  useEffect(() => {
    if (!opError) return;
    const timer = setTimeout(() => setOpError(null), 8000);
    return () => clearTimeout(timer);
  }, [opError]);
  const listRef = useRef<HTMLDivElement>(null);

  /** 看过的目录：再进去时先显示记住的列表（不用等），同时在后台重新读一次 */
  const cache = useRef(new Map<string, Listing>());
  useEffect(() => { cache.current.clear(); }, [loc]);
  const remember = (dir: string, result: Listing) => {
    const m = cache.current;
    for (const key of new Set([dir, result.path])) { m.delete(key); m.set(key, result); }
    while (m.size > CACHE_MAX) m.delete(m.keys().next().value as string);
  };
  /**
   * 读目录。force 为 false（进出目录）时有记住的列表就先显示它，后台再读一次、有变化再更新；
   * force 为 true（刷新、改动之后）时只显示读到的结果。
   */
  const load = useCallback(async (dir: string, keepSelection = false, force = keepSelection) => {
    const my = ++seq.current;
    const cached = force ? undefined : cache.current.get(dir);
    if (cached) {
      setListing(cached);
      setPath(cached.path);
      setError(null);
      if (!keepSelection) { setSelected(new Set()); anchor.current = null; }
    }
    setLoading(true);
    try {
      const result = await (await getFilesApi()).list(loc, dir);
      if (my !== seq.current) return;
      remember(dir, result);
      setListing(result);
      setPath(result.path);
      setError(null);
      if (!keepSelection && !cached) { setSelected(new Set()); anchor.current = null; }
    } catch (e) {
      if (my !== seq.current) return;
      // 后台重新读失败：先留着记住的列表，在底栏说明
      if (cached) { setOpError(t("files.refreshFailed", { reason: describeError(e, t("files.authHint")) })); return; }
      setPath(dir);
      setListing(null);
      // 浏览文件要另外建立一条 SSH 连接：用密码登录且没保存密码时无法自动登录
      setError(describeError(e, t("files.authHint")));
    } finally {
      if (my === seq.current) setLoading(false);
    }
  }, [loc, t]);

  // 家目录：终端还没报告当前目录时先显示它
  useEffect(() => {
    let alive = true;
    // 失败（服务器连不上、登录失败等）要显示出来，否则面板会一直停在“正在读取”
    void getFilesApi().then((api) => api.home(loc)).then((h) => { if (alive) setHome(h); }, (e: unknown) => {
      if (!alive) return;
      setError(describeError(e, t("files.authHint")));
    });
    return () => { alive = false; };
  }, [loc, t]);

  // 本机终端：提示符钩子报告的目录
  useEffect(() => { if (spec.kind === "local" && cwd) setTermDir(cwd); }, [spec.kind, cwd]);

  // 服务器终端：打开面板时、每次按回车后稍等一会儿查询（命令执行需要时间）。
  // 每次查询要新建一条 SSH 连接，所以同时最多一个；查询期间又按了回车，结束后再查一次（合并成一次）
  const cwdQuery = useRef({ busy: false, again: false, final: false, session: sessionId });
  cwdQuery.current.session = sessionId;
  /** @param final 查不到时是否认定“不支持跟随”（刚连上时 shell 可能还没启动，前几次查不到不算） */
  const queryCwd = useCallback(async (final: boolean) => {
    const q = cwdQuery.current;
    if (q.busy) { q.again = true; q.final ||= final; return; }
    q.busy = true;
    q.final = final;
    try {
      const api = await getFilesApi();
      do {
        q.again = false;
        const isFinal = q.final;
        const sid = q.session;
        if (!sid) break;
        const dir = await api.terminalCwd(sid).catch(() => null);
        if (q.session !== sid) continue;
        if (dir) { setTermDir(dir); setNoFollow(false); } else if (isFinal) setNoFollow(true);
      } while (q.again);
    } finally {
      q.busy = false;
    }
  }, []);
  useEffect(() => {
    if (spec.kind === "local" || !sessionId) return;
    // 刚打开：登录和 shell 启动需要时间，分几次查；之后每次回车稍等一会儿再查
    const delays = commands === 0 ? [300, 1500, 4000] : [600];
    const timers = delays.map((ms, i) => setTimeout(() => void queryCwd(i === delays.length - 1), ms));
    return () => timers.forEach(clearTimeout);
  }, [spec.kind, sessionId, commands, queryCwd]);

  // 跟随终端目录；终端目录还不知道时显示家目录
  const wanted = follow ? termDir ?? home : null;
  useEffect(() => {
    if (wanted && wanted !== path) void load(wanted);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [wanted]);

  // 传输完成：目标是这里就刷新
  useEffect(() => onDirChanged((key, dir) => { if (key === locKey(loc) && dir === path) void load(dir, true); }), [loc, path, load]);

  // 可以把别的面板的文件拖进来
  const targetRef = useRef({ loc, home, path, title });
  targetRef.current = { loc, home, path, title };
  useEffect(() => registerDropTarget(
    paneId,
    () => (targetRef.current.path ? { paneId, loc: targetRef.current.loc, home: targetRef.current.home, dir: targetRef.current.path, title: targetRef.current.title } : null),
    (p, target) => requestDrop(p, target),
  ), [paneId]);

  const entries = useMemo(() => sortEntries((listing?.entries ?? []).filter((e) => showHidden || !e.hidden)), [listing, showHidden]);
  const hiddenCount = useMemo(() => (listing?.entries ?? []).filter((e) => e.hidden).length, [listing]);

  /** 手动进入目录：暂停跟随（进入的正好是终端目录时恢复） */
  const go = (dir: string) => {
    setFollow(dir === termDir);
    void load(dir);
  };
  const open = (e: FileEntry) => {
    if (!path) return;
    if (e.kind === "dir" || (e.kind === "symlink" && e.targetIsDir === true)) { go(joinPath(path, e.name)); return; }
    if (e.kind === "file" || (e.kind === "symlink" && e.targetIsDir === false)) {
      // 图片、压缩包等：不在软件里编辑；本机用默认程序打开，服务器上提示
      if (isBinaryName(e.name)) {
        if (loc.kind === "local") void act((api) => api.openDefault(joinPath(path, e.name)));
        else setOpError(t("files.binaryRemote", { name: e.name }));
        return;
      }
      setEditing(joinPath(path, e.name));
      return;
    }
    // 不知道指向什么的链接：先试着列一下，是目录才进入
    if (e.kind === "symlink" && e.targetIsDir === null) {
      const target = joinPath(path, e.name);
      void getFilesApi().then((api) => api.list(loc, target)).then(() => go(target), () => {});
    }
  };
  const backToTerminal = () => {
    setFollow(true);
    if (termDir ?? home) void load((termDir ?? home)!);
  };

  /** 执行一个文件操作：失败时在面板顶部显示原因；成功后刷新列表并选中 select */
  const act = async (fn: (api: Awaited<ReturnType<typeof getFilesApi>>) => Promise<unknown>, select?: string) => {
    try {
      await fn(await getFilesApi());
      setOpError(null);
      if (path) await load(path, true);
      if (select) { setSelected(new Set([select])); anchor.current = select; }
    } catch (e) {
      setOpError(formatError(e));
    } finally {
      listRef.current?.focus();
    }
  };
  const commitRename = (from: string, to: string) => {
    setRenaming(null);
    const name = to.trim();
    if (!path || !name || name === from) { listRef.current?.focus(); return; }
    void act((api) => api.rename(loc, joinPath(path, from), joinPath(path, name)), name);
  };
  const commitCreate = (kind: "dir" | "file", value: string) => {
    setCreating(null);
    const name = value.trim();
    if (!path || !name) { listRef.current?.focus(); return; }
    const full = joinPath(path, name);
    void act((api) => (kind === "dir" ? api.createDir(loc, full) : api.createFile(loc, full)), name);
  };
  const local = loc.kind === "local";
  const isDirEntry = (e: FileEntry) => e.kind === "dir" || (e.kind === "symlink" && e.targetIsDir === true);
  /** 复制到剪贴板（多个路径每行一个） */
  const copy = (text: string) => void writeClipboardText(text);
  /** 空白处的菜单：新建、刷新、复制当前路径 */
  const backgroundMenu = (dir: string): MenuEntry[] => [
    { id: "new-dir", label: t("files.newFolder"), icon: <FolderPlus size={13} />, onSelect: () => setCreating("dir") },
    { id: "new-file", label: t("files.newFile"), icon: <FilePlus size={13} />, onSelect: () => setCreating("file") },
    { kind: "separator", id: "s1" },
    { id: "refresh", label: t("common.refresh"), shortcut: "F5", onSelect: () => void load(dir, true) },
    { id: "copy-path", label: t("files.copyCurrentPath"), onSelect: () => copy(dir) },
    ...(local ? [{ id: "explorer", label: t("files.openInExplorer"), onSelect: () => void act((api) => api.openDefault(dir)) }] : []),
  ];
  /** 文件或文件夹上的菜单（names 为选中的项） */
  const itemMenu = (dir: string, names: string[]): MenuEntry[] => {
    const items = entries.filter((e) => names.includes(e.name));
    const one = items.length === 1 ? items[0] : null;
    const full = (n: string) => joinPath(dir, n);
    const entriesOut: MenuEntry[] = [];
    if (one && isDirEntry(one)) entriesOut.push({ id: "open", label: t("files.openFolder"), shortcut: "Enter", onSelect: () => open(one) });
    if (one && !isDirEntry(one) && !isBinaryName(one.name)) entriesOut.push({ id: "edit", label: t("files.editInApp"), shortcut: "Enter", onSelect: () => setEditing(full(one.name)) });
    if (one && local) {
      if (!isDirEntry(one)) entriesOut.push({ id: "open-default", label: t("files.openDefault"), onSelect: () => void act((api) => api.openDefault(full(one.name))) });
      entriesOut.push({ id: "reveal", label: t("files.reveal"), onSelect: () => void act((api) => api.reveal(full(one.name))) });
    }
    if (entriesOut.length) entriesOut.push({ kind: "separator", id: "s0" });
    entriesOut.push(
      { id: "copy-path", label: items.length > 1 ? t("files.copyPaths", { count: items.length }) : t("files.copyPath"), shortcut: "Ctrl Shift C", onSelect: () => copy(items.map((e) => full(e.name)).join("\n")) },
      { id: "copy-name", label: items.length > 1 ? t("files.copyNames", { count: items.length }) : t("files.copyName"), onSelect: () => copy(items.map((e) => e.name).join("\n")) },
      { kind: "separator", id: "s1" },
    );
    if (one) {
      entriesOut.push({ id: "rename", label: t("files.rename"), shortcut: "F2", onSelect: () => setRenaming(one.name) });
      if (!local) entriesOut.push({ id: "chmod", label: t("files.chmod"), onSelect: () => setChmodPath(full(one.name)) });
    }
    entriesOut.push(
      { id: "new-dir", label: t("files.newFolder"), onSelect: () => setCreating("dir") },
      { id: "new-file", label: t("files.newFile"), onSelect: () => setCreating("file") },
      { kind: "separator", id: "s2" },
      { id: "delete", label: items.length > 1 ? t("files.deleteMany", { count: items.length }) : t("files.delete"), shortcut: "Del", danger: true, onSelect: () => setDeleting(items.map((e) => full(e.name))) },
    );
    return entriesOut;
  };

  const click = (ev: MouseEvent, name: string) => {
    if (ev.shiftKey && anchor.current) {
      const names = entries.map((e) => e.name);
      const [a, b] = [names.indexOf(anchor.current), names.indexOf(name)].sort((x, y) => x - y);
      if (a >= 0) { setSelected(new Set(names.slice(a, b + 1))); return; }
    }
    if (ev.ctrlKey || ev.metaKey) {
      const next = new Set(selected);
      if (next.has(name)) next.delete(name); else next.add(name);
      setSelected(next);
    } else {
      setSelected(new Set([name]));
    }
    anchor.current = name;
  };
  const keyDown = (ev: KeyboardEvent<HTMLDivElement>) => {
    if (ev.key === "Backspace" && path) { const up = parentPath(path); if (up) { ev.preventDefault(); go(up); } }
    if (ev.key === "a" && (ev.ctrlKey || ev.metaKey)) { ev.preventDefault(); setSelected(new Set(entries.map((e) => e.name))); }
    if (ev.key === "Escape") setSelected(new Set());
    if (ev.key === "Enter" && selected.size === 1) { const e = entries.find((x) => selected.has(x.name)); if (e) open(e); }
    if (ev.key === "F2" && selected.size === 1) { ev.preventDefault(); setRenaming([...selected][0]); }
    if (ev.key === "F5" && path) { ev.preventDefault(); void load(path, true); }
    if (ev.key === "Delete" && selected.size > 0 && path) { ev.preventDefault(); setDeleting(entries.filter((e) => selected.has(e.name)).map((e) => joinPath(path, e.name))); }
    if (ev.key.toLowerCase() === "c" && ev.ctrlKey && ev.shiftKey && path) {
      ev.preventDefault();
      copy(selected.size ? entries.filter((e) => selected.has(e.name)).map((e) => joinPath(path, e.name)).join("\n") : path);
    }
  };

  /** 按下文件行准备拖动：拖的是已选中的那些（按在未选中的行上则只拖这一行） */
  const dragFrom = (name: string): DragPayload | null => {
    if (!path) return null;
    const names = selected.has(name) ? entries.filter((e) => selected.has(e.name)).map((e) => e.name) : [name];
    if (!selected.has(name)) { setSelected(new Set([name])); anchor.current = name; }
    return { paneId, loc, home, dir: path, names };
  };

  // 传入/传出这个目录的传输：底部显示进度（总进度也在标题栏的“传输”按钮里）
  const jobs = useTransfers();
  const here = locKey(loc);
  const moving = jobs
    .filter((j) => j.state === "running")
    .flatMap((j) => {
      const into = locKey(j.req.to) === here && j.req.destDir === path;
      const from = locKey(j.req.from) === here && j.req.sources.some((x) => path !== null && parentPath(x) === path);
      if (!into && !from) return [];
      const p = j.progress;
      const percent = p && p.totalBytes > 0 ? Math.min(100, Math.round((p.doneBytes / p.totalBytes) * 100)) : 0;
      // 当前文件名：还没有进度时用要传的第一项的名字
      const name = p?.current ? baseName(p.current) : baseName(j.req.sources[0] ?? "");
      // 进度条是整个任务的；一次传多个文件时标出正在传第几个，免得以为是这个文件的进度
      const nth = p && p.filesTotal > 1 ? ` · ${Math.min(p.filesDone + 1, p.filesTotal)}/${p.filesTotal}` : "";
      return [{ key: j.key, into, name: name + nth, percent }];
    });

  const isOver = drag && drag.over === paneId && drag.payload.paneId !== paneId && !!path;
  const following = follow && !!termDir && path === termDir;

  return (
    <div className={`files${isOver ? " files--over" : ""}`} data-file-drop={paneId} aria-label={t("files.panelLabel", { name: title })} role="region">
      <div className="files__head">
        <div className="files__crumbs" title={path ?? ""}>
          {path && crumbs(path).map((c, i, all) => (
            <span key={c.path} className="files__crumb-wrap">
              {i > 0 && <ChevronRight size={11} className="files__sep" aria-hidden="true" />}
              <button type="button" className={`files__crumb${i === all.length - 1 ? " files__crumb--cur" : ""}`} onClick={() => go(c.path)}>{c.name}</button>
            </span>
          ))}
        </div>
        <div className="files__tools">
          <button type="button" className="pane__btn" title={t("common.refresh")} aria-label={t("common.refresh")} disabled={!path} onClick={() => path && void load(path, true)}><RefreshCw size={13} className={loading ? "files__spin" : ""} /></button>
          <button type="button" className={`pane__btn${showHidden ? " pane__btn--on" : ""}`} aria-pressed={showHidden} title={t("files.showHidden")} aria-label={t("files.showHidden")} onClick={() => setShowHidden(!showHidden)}>
            {showHidden ? <Eye size={14} /> : <EyeOff size={14} />}
          </button>
          <button type="button" className={`pane__btn${following ? " pane__btn--on" : ""}${noFollow && spec.kind !== "local" && follow ? " files__follow--off" : ""}`} aria-pressed={following} title={noFollow ? `${t("files.followUnavailable")}\n${t("files.followUnavailableHint")}` : t("files.follow")} aria-label={t("files.follow")} disabled={!termDir && !home} onClick={backToTerminal}>
            <LocateFixed size={14} />
          </button>
          <button type="button" className="pane__btn" title={t("common.close")} aria-label={t("common.close")} onClick={onClose}><X size={14} /></button>
        </div>
      </div>
      {/* 跟不了终端目录时只在“跟随”按钮上标出来（不另起一行，列表不下移） */}
      <div
        className="files__list"
        role="listbox"
        aria-multiselectable="true"
        aria-label={path ?? ""}
        tabIndex={0}
        onKeyDown={keyDown}
        onClick={(e) => { if (e.target === e.currentTarget) setSelected(new Set()); }}
        ref={listRef}
        onContextMenu={(e) => {
          if (!path) return;
          // 在空白处右键：清掉选择，只给新建、刷新等
          setSelected(new Set());
          ctx.open(e, backgroundMenu(path));
        }}
      >
        {/* 上一级：永远在列表最上面（空文件夹、出错、加载中也一样）；和普通文件夹一样双击进入 */}
        {path && parentPath(path) && (
          <div
            role="option"
            aria-selected={false}
            className="files__row files__row--dir files__row--up"
            title={t("files.up")}
            onClick={() => setSelected(new Set())}
            onDoubleClick={() => go(parentPath(path)!)}
          >
            <Folder size={14} className="files__icon" aria-hidden="true" />
            <span className="files__name">..</span>
            <span className="files__size" />
            <span className="files__time" />
          </div>
        )}
        {creating && (
          <div className="files__row files__row--edit">
            {creating === "dir" ? <Folder size={14} className="files__icon" aria-hidden="true" /> : <File size={14} className="files__icon" aria-hidden="true" />}
            <NameInput
              initial=""
              placeholder={creating === "dir" ? t("files.newFolderName") : t("files.newFileName")}
              onDone={(v) => (v === null ? (setCreating(null), listRef.current?.focus()) : commitCreate(creating, v))}
            />
          </div>
        )}
        {error && <div className="files__empty files__empty--bad" role="alert">{error}</div>}
        {!error && listing && entries.length === 0 && <div className="files__empty">{hiddenCount && !showHidden ? t("files.onlyHidden", { count: hiddenCount }) : t("files.empty")}</div>}
        {!error && !listing && <div className="files__empty" role="status">{t("files.loading")}</div>}
        {entries.map((e) => {
          const dirLike = e.kind === "dir" || (e.kind === "symlink" && e.targetIsDir === true);
          const Icon = e.kind === "dir" ? Folder : e.kind === "symlink" ? (e.targetIsDir ? FolderSymlink : Link2) : File;
          return (
            <div
              key={e.name}
              role="option"
              aria-selected={selected.has(e.name)}
              className={`files__row${e.hidden ? " files__row--hidden" : ""}${dirLike ? " files__row--dir" : ""}`}
              title={renaming === e.name ? undefined : e.name}
              onClick={(ev) => { if (renaming !== e.name) click(ev, e.name); }}
              onDoubleClick={() => { if (renaming !== e.name) open(e); }}
              onPointerDown={(ev) => { if (renaming !== e.name && ev.button === 0) beginDrag(ev, () => dragFrom(e.name)); }}
              onContextMenu={(ev) => {
                if (!path) return;
                ev.stopPropagation();
                // 在没选中的项上右键：只选它
                const names = selected.has(e.name) ? [...selected] : [e.name];
                if (!selected.has(e.name)) { setSelected(new Set([e.name])); anchor.current = e.name; }
                ctx.open(ev, itemMenu(path, names));
              }}
            >
              <Icon size={14} className="files__icon" aria-hidden="true" />
              {renaming === e.name
                ? <NameInput initial={e.name} onDone={(v) => (v === null ? (setRenaming(null), listRef.current?.focus()) : commitRename(e.name, v))} />
                : <span className="files__name pl-ellipsis">{e.name}</span>}
              {renaming !== e.name && <span className="files__size">{e.kind === "file" ? formatSize(e.size) : ""}</span>}
              {renaming !== e.name && <span className="files__time">{formatTime(e.mtime)}</span>}
            </div>
          );
        })}
      </div>
      {moving.length > 0 && (
        <div className="files__xfers" role="status" aria-label={t("files.transfers")}>
          {moving.slice(0, 3).map((m) => (
            <div key={m.key} className="files__xfer" title={m.into ? t("files.incoming", { name: m.name }) : t("files.outgoing", { name: m.name })}>
              {m.into ? <ArrowDownToLine size={12} aria-label={t("files.incomingShort")} /> : <ArrowUpFromLine size={12} aria-label={t("files.outgoingShort")} />}
              <span className="files__xfer-name mono">{m.name}</span>
              <span className="files__xfer-bar"><span style={{ width: `${m.percent}%` }} /></span>
              <span className="files__xfer-pct">{m.percent}%</span>
            </div>
          ))}
          {moving.length > 3 && <div className="files__xfer files__xfer--more">{t("files.andMore", { count: moving.length - 3 })}</div>}
        </div>
      )}
      <div className="files__foot">
        {/* 操作失败写在底栏（替换计数，几秒后恢复），列表不被推下去 */}
        {opError ? <span className="files__op-error pl-ellipsis" role="alert" title={opError}>{opError}</span> : listing && (
          <span>
            {t("files.count", { count: listing.entries.length })}
            {hiddenCount > 0 && ` · ${t("files.hiddenCount", { count: hiddenCount })}`}
            {selected.size > 0 && ` · ${t("files.selectedCount", { count: selected.size })}`}
            {listing.truncated && <span className="files__warn">{` · ${t("files.truncated")}`}</span>}
          </span>
        )}
        <span className="files__hint">{t("files.dragHint")}</span>
      </div>
      {isOver && <div className="files__drop" aria-hidden="true">{t("files.dropHere", { dir: path })}</div>}
      {ctx.node}
      {editing && (
        <TextEditor
          loc={loc}
          path={editing}
          where={loc.kind === "local" ? t("files.thisPc") : title}
          onClose={() => { setEditing(null); listRef.current?.focus(); }}
          onSaved={() => { if (path) void load(path, true); }}
        />
      )}
      {deleting && (
        <DeleteDialog
          loc={loc}
          paths={deleting}
          onClose={() => setDeleting(null)}
          onDone={(err) => { setDeleting(null); setOpError(err); if (path) void load(path, false, true); }}
        />
      )}
      {chmodPath && (
        <ChmodDialog
          loc={loc}
          path={chmodPath}
          onClose={() => setChmodPath(null)}
          onDone={(err) => { setChmodPath(null); setOpError(err); }}
        />
      )}
    </div>
  );
}

/**
 * 原地改名、新建时的名字输入框：Enter 确定，Esc 取消，点到别处也算确定。
 * 改名时默认选中扩展名前面的部分（和资源管理器一样）。
 */
function NameInput({ initial, placeholder, onDone }: { initial: string; placeholder?: string; onDone: (value: string | null) => void }) {
  const ref = useRef<HTMLInputElement>(null);
  const done = useRef(false);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.focus();
    const dot = initial.lastIndexOf(".");
    el.setSelectionRange(0, dot > 0 ? dot : initial.length);
  }, [initial]);
  const finish = (v: string | null) => {
    if (done.current) return;
    done.current = true;
    onDone(v);
  };
  return (
    <input
      ref={ref}
      className="files__name-input mono"
      defaultValue={initial}
      placeholder={placeholder}
      spellCheck={false}
      onClick={(e) => e.stopPropagation()}
      onDoubleClick={(e) => e.stopPropagation()}
      onPointerDown={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter") { e.preventDefault(); finish(e.currentTarget.value); }
        if (e.key === "Escape") { e.preventDefault(); finish(null); }
      }}
      onBlur={(e) => finish(e.currentTarget.value)}
    />
  );
}
