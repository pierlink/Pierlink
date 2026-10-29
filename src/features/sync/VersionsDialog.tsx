import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Dialog, TextField } from "../../components";
import { formatError } from "../../errors";
import { formatBytes } from "./format";
import { getSyncApi, type TrashEndpoint, type VersionRecord } from "./syncApi";
import "./sync.css";

function when(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/**
 * 回收站占用：每端一行（本机、各服务器），可以立即清空。
 * 确认就地换掉按钮，结果写在下方固定高度的状态行里，对话框不跳动
 */
function TrashSection({ taskId }: { taskId: string }) {
  const { t } = useTranslation();
  const [rows, setRows] = useState<TrashEndpoint[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [status, setStatus] = useState<{ tone: "ok" | "danger"; text: string } | null>(null);
  const keyOf = (r: TrashEndpoint) => r.serverId ?? "";
  const nameOf = (r: TrashEndpoint) => (r.serverId === null ? t("versions.trashLocal") : r.name);
  const load = () => {
    void getSyncApi().then((api) => api.trash(taskId)).then(
      (l) => { setRows(l); setLoadError(null); },
      (e: unknown) => setLoadError(formatError(e)),
    );
  };
  useEffect(load, [taskId]); // eslint-disable-line react-hooks/exhaustive-deps
  const clear = async (r: TrashEndpoint) => {
    setConfirming(null);
    setBusy(keyOf(r));
    try {
      const done = await (await getSyncApi()).clearTrash(taskId, r.serverId);
      setStatus({ tone: "ok", text: t("versions.trashCleared", { name: nameOf(r), size: formatBytes(done.bytes), count: done.files }) });
    } catch (e) {
      setStatus({ tone: "danger", text: t("versions.trashFailed", { name: nameOf(r), reason: formatError(e) }) });
    } finally {
      setBusy(null);
      load();
    }
  };
  return (
    <section className="trash" aria-label={t("versions.trashTitle")}>
      <div className="trash__head">
        <strong>{t("versions.trashTitle")}</strong>
        <span className="sync-muted">{t("versions.trashHint")}</span>
      </div>
      {loadError && <span className="trash__error">{loadError}</span>}
      {!loadError && rows === null && <span role="status" className="sync-muted">{t("common.loading")}</span>}
      {rows?.map((r) => {
        const k = keyOf(r);
        const empty = (r.files ?? 0) === 0;
        return (
          <div key={k} className="trash__row">
            <span className="trash__name pl-ellipsis" title={nameOf(r)}>{nameOf(r)}</span>
            <span className="trash__usage pl-ellipsis" title={r.error ? formatError(r.error) : undefined}>
              {r.error ? <span className="trash__error">{formatError(r.error)}</span>
                : empty ? t("versions.trashEmpty")
                : t("versions.trashUsage", { size: formatBytes(r.bytes ?? 0), count: r.files ?? 0 })}
            </span>
            <span className="trash__action">
              {confirming === k ? (
                <>
                  <Button size="sm" variant="danger" onClick={() => void clear(r)}>{t("versions.trashClear")}</Button>
                  <Button size="sm" variant="ghost" onClick={() => setConfirming(null)}>{t("common.cancel")}</Button>
                </>
              ) : (
                <Button size="sm" disabled={!!r.error || empty || busy !== null} onClick={() => setConfirming(k)}>
                  {busy === k ? t("versions.trashClearing") : t("versions.trashClear")}
                </Button>
              )}
            </span>
          </div>
        );
      })}
      {confirming !== null && rows && (() => {
        const r = rows.find((x) => keyOf(x) === confirming);
        return r ? <span className="trash__status trash__status--warn" role="alert">{t("versions.trashConfirmBody", { size: formatBytes(r.bytes ?? 0), count: r.files ?? 0 })}</span> : null;
      })()}
      {confirming === null && <span className={`trash__status${status ? ` trash__status--${status.tone}` : ""}`} role="status">{status?.text ?? ""}</span>}
    </section>
  );
}

/**
 * 浏览并恢复旧版本（被同步覆盖或删除的文件）。
 * 恢复时如果当前文件存在，它会先被保存为新的旧版本，所以恢复本身不会丢内容。
 */
export function VersionsDialog({ taskId, taskName, onClose }: { taskId: string; taskName: string; onClose: () => void }) {
  const { t } = useTranslation();
  const [list, setList] = useState<VersionRecord[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<{ ok: number; failed: { path: string; reason: string }[] } | null>(null);

  const key = (v: VersionRecord) => `${v.path}\u0000${v.versionTimeMs}`;
  const load = () => {
    setError(null);
    void getSyncApi().then((api) => api.versions(taskId)).then(
      (l) => setList([...l].sort((a, b) => b.versionTimeMs - a.versionTimeMs)),
      (e: unknown) => setError(formatError(e)),
    );
  };
  useEffect(load, [taskId]); // eslint-disable-line react-hooks/exhaustive-deps

  const shown = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return (list ?? []).filter((v) => !q || v.path.toLowerCase().includes(q)).slice(0, 500);
  }, [list, filter]);

  const restore = async () => {
    const picks = (list ?? []).filter((v) => picked.has(key(v))).map((v) => ({ path: v.path, versionTimeMs: v.versionTimeMs }));
    setBusy(true);
    try {
      const failed = await (await getSyncApi()).restoreVersions(taskId, picks);
      setResult({ ok: picks.length - failed.length, failed });
      setPicked(new Set());
      load();
    } catch (e) {
      setError(formatError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      title={t("versions.title", { name: taskName })}
      size="lg"
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("deploy.close")}</Button>
        <Button variant="primary" disabled={busy || picked.size === 0} onClick={() => void restore()}>
          {busy ? t("versions.restoring") : t("versions.restore", { count: picked.size })}
        </Button>
      </>}
    >
      <div className="versions">
        <TrashSection taskId={taskId} />
        <p className="wiz-hint">{t("versions.hint")}</p>
        <TextField label={t("versions.filter")} value={filter} onChange={(e) => setFilter(e.target.value)} />
        {error && <Banner tone="danger" title={t("versions.loadFailed")}>{error}</Banner>}
        {result && (
          <Banner tone={result.failed.length ? "warn" : "ok"} title={t("versions.restored", { count: result.ok })}>
            {result.failed.map((f) => `${f.path}：${f.reason}`).join("；")}
          </Banner>
        )}
        {list === null && !error && <span role="status" className="sync-muted">{t("common.loading")}</span>}
        {list !== null && list.length === 0 && <span className="sync-muted">{t("versions.empty")}</span>}
        {shown.length > 0 && (
          <div className="wiz-list versions__list">
            {shown.map((v) => (
              <label key={key(v)} className="wiz-list__row versions__row">
                <input
                  type="checkbox"
                  checked={picked.has(key(v))}
                  onChange={(e) => setPicked((s) => { const n = new Set(s); if (e.target.checked) n.add(key(v)); else n.delete(key(v)); return n; })}
                />
                <span className="mono">{v.path}</span>
                <span className="sync-muted">{t("versions.savedAt", { time: when(v.versionTimeMs) })} · {formatBytes(v.size)}</span>
              </label>
            ))}
          </div>
        )}
      </div>
    </Dialog>
  );
}
