import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { formatBytes, formatCount } from "./format";
import { getSyncApi, type ConflictRecord, type SyncEventRecord, type SyncStatusRecord, type SyncTaskRecord } from "./syncApi";
import type { SyncConflictView, SyncEventView, SyncTaskView } from "./types";

const CONFLICT_POLL_MS = 30000;

function hms(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  const clock = `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  return d.toDateString() === new Date().toDateString() ? clock : `${p(d.getMonth() + 1)}-${p(d.getDate())} ${clock}`;
}

/** 一页事件的条数（和后端一致） */
const EVENTS_PAGE = 200;

/** 最新一页和已经加载的更早几页合在一起，按编号去重，新的在前 */
function mergeEvents(latest: SyncEventRecord[] | undefined, older: SyncEventRecord[] | undefined): SyncEventRecord[] {
  const seen = new Set<number>();
  return [...(latest ?? []), ...(older ?? [])].filter((e) => (seen.has(e.id) ? false : (seen.add(e.id), true)));
}

const EVENT_KIND: Partial<Record<SyncEventRecord["kind"], SyncEventView["kind"]>> = {
  downloaded: "down", uploaded: "up", conflict: "conflict", error: "error", deleted: "deleted", deviceConnected: "link", deviceDisconnected: "link",
};

function toEvents(list: SyncEventRecord[] | undefined, tr: (k: string) => string): SyncEventView[] {
  return (list ?? [])
    .filter((e) => EVENT_KIND[e.kind])
    .map((e) => ({
      id: e.id,
      time: hms(e.timeMs),
      by: e.by === "local" ? tr("sync.byLocal") : e.by,
      kind: EVENT_KIND[e.kind]!,
      server: e.server,
      text: e.path ?? (e.kind === "deviceConnected" ? tr("sync.event.connected") : e.kind === "deviceDisconnected" ? tr("sync.event.disconnected") : e.detail ?? ""),
    }));
}

function toConflicts(list: ConflictRecord[] | undefined): SyncConflictView[] {
  return (list ?? []).map((c) => ({ record: c, path: c.path, otherName: c.conflictFile.split(/[\\/]/).pop() ?? c.conflictFile, from: c.fromDevice ?? "" }));
}

const POLL_MS = 4000;

/** 任务记录 + 实时状态 → 界面视图 */
function toView(
  t: SyncTaskRecord,
  s: SyncStatusRecord | undefined,
  tr: (k: string, o?: Record<string, unknown>) => string,
  lang: string,
  events: SyncEventRecord[] | undefined,
  conflicts: ConflictRecord[] | undefined,
): SyncTaskView {
  const anyOffline = !!s && s.servers.some((x) => !x.connected);
  const passwordServer = s?.servers.find((x) => !x.connected && x.needsPassword);
  const loginServer = s?.servers.find((x) => !x.connected && x.needsLogin);
  const percent = s && s.bytes > 0 ? Math.max(0, Math.min(100, Math.round(((s.bytes - s.needBytes) / s.bytes) * 100))) : 100;
  let state: SyncTaskView["state"];
  let detail: string;
  if (s?.deletionGuardPaused) {
    state = "error";
    detail = tr("sync.detail.guardPaused");
  } else if (t.paused) {
    state = "paused";
    detail = tr("sync.detail.paused");
  } else if (!s || s.state === "busy") {
    state = "waiting";
    detail = tr("sync.detail.loading");
  } else if (s.state === "stopped") {
    state = "error";
    detail = tr("sync.detail.localStopped");
  } else if (s.state === "error" || s.errors.length > 0) {
    state = "error";
    detail = s.errors.length > 0 ? tr("sync.detail.fileErrors", { count: s.errors.length }) : tr("sync.detail.folderError");
  } else if (passwordServer) {
    state = "error";
    detail = tr("sync.detail.needPassword", { name: t.endpoints.find((e) => e.serverId === passwordServer.serverId)?.serverName ?? "" });
  } else if (anyOffline) {
    state = "error";
    detail = tr("sync.detail.serverOffline");
  } else if (conflicts && conflicts.length > 0) {
    state = "conflict";
    detail = tr("sync.detail.conflicts", { count: conflicts.length });
  } else if (s.state === "syncing" || s.state.startsWith("sync") || s.needBytes > 0) {
    state = "syncing";
    detail = tr("sync.detail.syncing", { size: formatBytes(s.needBytes) });
  } else if (s.state === "scanning") {
    state = "waiting";
    detail = tr("sync.detail.scanning");
  } else {
    state = "synced";
    detail = tr("sync.detail.synced", { n: formatCount(s.files, lang), size: formatBytes(s.bytes) });
  }
  return {
    id: t.id,
    name: t.name,
    localPath: t.localPath,
    endpoints: t.endpoints.map((e) => {
      const st = s?.servers.find((x) => x.serverId === e.serverId);
      return { serverId: e.serverId, serverName: e.serverName, remotePath: e.remotePath, mode: e.mode, link: !s || s.state === "busy" ? "reconnecting" : st?.connected ? "online" : st?.needsPassword ? "password" : "offline" };
    }),
    state,
    percent: t.paused ? 100 : percent,
    detail,
    passwordServerId: t.paused ? null : passwordServer?.serverId ?? null,
    loginServerId: t.paused ? null : loginServer?.serverId ?? null,
    files: s?.files ?? 0,
    bytes: s?.bytes ?? 0,
    excludedFiles: null,
    excludedBytes: null,
    versionsBytes: t.versioning ? -1 : null,
    ignoreRules: t.ignoreRules,
    collaboration: t.collaboration,
    watchDelayS: t.watchDelayS,
    conflictMode: t.conflictMode,
    conflicts: toConflicts(conflicts),
    events: toEvents(events, tr),
    eventsHasMore: false,
    errors: s?.errors ?? [],
    guardPaused: !!s?.deletionGuardPaused,
    localMode: t.localMode ?? "sendReceive",
    localChanges: s?.localChanges ?? 0,
  };
}

/**
 * 同步任务列表：读取任务，定时查询实时状态。
 * @returns 视图列表、读取错误、刷新函数
 */
export function useSyncTasks() {
  const { t, i18n } = useTranslation();
  const [records, setRecords] = useState<SyncTaskRecord[]>([]);
  const [statuses, setStatuses] = useState<Record<string, SyncStatusRecord>>({});
  const [events, setEvents] = useState<Record<string, SyncEventRecord[]>>({});
  // 点“加载更多”取到的更早的事件（最新一页仍按 4 秒刷新）；还有没有更早的
  const [olderEvents, setOlderEvents] = useState<Record<string, SyncEventRecord[]>>({});
  const [noMore, setNoMore] = useState<Record<string, boolean>>({});
  const [conflicts, setConflicts] = useState<Record<string, ConflictRecord[]>>({});
  const [conflictTick, setConflictTick] = useState(0);
  const [loadError, setLoadError] = useState<unknown>(null);
  const alive = useRef(true);
  // 开发模式下组件会先挂载、卸载再挂载一次，所以挂载时要重新置为 true
  useEffect(() => { alive.current = true; return () => { alive.current = false; }; }, []);

  const reload = useCallback(async () => {
    try {
      const list = await (await getSyncApi()).tasks();
      if (alive.current) { setRecords(list); setLoadError(null); }
    } catch (e) {
      if (alive.current) setLoadError(e);
    }
  }, []);

  useEffect(() => { void reload(); }, [reload]);

  // 定时查询每个任务的状态；同步服务正忙（busy）时保留上一次的结果
  useEffect(() => {
    let stop = false;
    const poll = async () => {
      const api = await getSyncApi();
      for (const r of records) {
        if (stop) return;
        try {
          const s = await api.status(r.id);
          if (!stop && s.state !== "busy") setStatuses((m) => ({ ...m, [r.id]: s }));
          const ev = await api.events(r.id);
          if (!stop) setEvents((m) => ({ ...m, [r.id]: ev }));
        } catch {
          // 单个任务查询失败不影响其他任务；下一轮再试
        }
      }
    };
    void poll();
    const timer = setInterval(() => void poll(), POLL_MS);
    return () => { stop = true; clearInterval(timer); };
  }, [records]);

  // 冲突列表要扫描本机目录，频率低一些；处理冲突后可立即刷新
  useEffect(() => {
    let stop = false;
    const poll = async () => {
      const api = await getSyncApi();
      for (const r of records) {
        if (stop) return;
        try {
          const c = await api.conflicts(r.id);
          if (!stop) setConflicts((m) => ({ ...m, [r.id]: c }));
        } catch {
          // 目录暂时不可读时保留上一次的结果
        }
      }
    };
    void poll();
    const timer = setInterval(() => void poll(), CONFLICT_POLL_MS);
    return () => { stop = true; clearInterval(timer); };
  }, [records, conflictTick]);

  const views = records.map((r) => {
    const merged = mergeEvents(events[r.id], olderEvents[r.id]);
    const view = toView(r, statuses[r.id], t, i18n.language, merged, conflicts[r.id]);
    return { ...view, eventsHasMore: !noMore[r.id] && merged.length >= EVENTS_PAGE };
  });
  /** 加载更早的一页事件 */
  const loadMoreEvents = async (taskId: string) => {
    const merged = mergeEvents(events[taskId], olderEvents[taskId]);
    const last = merged[merged.length - 1];
    if (!last) return;
    const page = await (await getSyncApi()).events(taskId, { timeMs: last.timeMs, id: last.id });
    setOlderEvents((m) => ({ ...m, [taskId]: [...(m[taskId] ?? []), ...page] }));
    if (page.length < EVENTS_PAGE) setNoMore((m) => ({ ...m, [taskId]: true }));
  };
  return { tasks: views, loadError, reload, loadMoreEvents, refreshConflicts: () => setConflictTick((n) => n + 1) };
}
