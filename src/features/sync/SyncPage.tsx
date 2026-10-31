import { loginServer } from "../servers/login";
import { useEffect, useMemo, useState, type ReactElement } from "react";
import { useTranslation } from "react-i18next";
import { ArrowDownLeft, ArrowLeft, ArrowLeftRight, ArrowRight, ArrowUpRight, Pencil, CircleAlert, FolderOpen, Link2, MoreHorizontal, Pause, Play, Plus, RefreshCw, Server, Trash2, TriangleAlert, X } from "lucide-react";
import { Badge, Banner, Button, IconButton, Notice, Segmented, SplitButton, useContextMenu } from "../../components";
import { formatBytes, formatCount } from "./format";
import { directionOf } from "./direction";
import type { SyncConflictView, SyncEndpointView, SyncEventView, SyncTaskState, SyncTaskView } from "./types";
import "./sync.css";
import { formatError } from "../../errors";
import { getSyncApi } from "./syncApi";
import { getFilesApi } from "../files/api";
import { ConflictDiffDialog } from "./ConflictDiffDialog";
import { passwordBus } from "../passwords/bus";

type Filter = "all" | "syncing" | "conflict" | "error" | "paused";

/** 任务状态 → 颜色语气 */
const tone: Record<SyncTaskState, "ok" | "accent" | "warn" | "danger" | "neutral"> = {
  synced: "ok", syncing: "accent", waiting: "accent", conflict: "warn", error: "danger", paused: "neutral",
};

function matches(task: SyncTaskView, f: Filter): boolean {
  if (f === "all") return true;
  if (f === "syncing") return task.state === "syncing" || task.state === "waiting";
  return task.state === f;
}

/** 规则是否属于“敏感文件”类（默认不同步，用醒目颜色标出） */
function isSensitiveRule(rule: string): boolean {
  return /(^|\))(\.env|\*\.pem|\*\.key)$/.test(rule);
}

function TaskRow({ task, selected, onSelect }: { task: SyncTaskView; selected: boolean; onSelect: () => void }) {
  const { t } = useTranslation();
  const servers = task.endpoints.length > 1
    ? t("sync.serversMore", { name: task.endpoints[0].serverName, count: task.endpoints.length, more: task.endpoints.length - 1 })
    : task.endpoints[0]?.serverName ?? "";
  return (
    <button type="button" className="task-row" aria-current={selected} onClick={onSelect}>
      <span className="task-row__head">
        <span className="task-row__name">{task.name}</span>
        <span className="task-row__server">{servers}</span>
        <span className="task-row__spacer" />
        <Badge tone={tone[task.state]}>{t(`sync.state.${task.state}`, { percent: task.percent, count: task.conflicts.length })}</Badge>
      </span>
      <span className="task-row__paths mono">
        <span title={task.localPath}>{task.localPath}</span>
        {(() => { const d = directionOf(task.localMode, task.endpoints.map((e) => e.mode)); const Icon = d === "toServer" ? ArrowRight : d === "toLocal" ? ArrowLeft : ArrowLeftRight; return <Icon size={12} aria-hidden />; })()}
        <span title={task.endpoints[0]?.remotePath}>{task.endpoints[0]?.remotePath}</span>
      </span>
      <span className={`bar task-row__bar task-row__bar--${tone[task.state]}`}><span style={{ width: `${task.percent}%` }} /></span>
      <span className="task-row__detail" title={task.detail}>{task.detail}</span>
    </button>
  );
}

function EndpointRow({ ep, localMode, onRemove }: { ep: SyncEndpointView; localMode: SyncTaskView["localMode"]; onRemove?: () => void }) {
  const { t } = useTranslation();
  const link = ep.link === "reconnecting" ? t("sync.link.reconnecting", { s: ep.retryInS ?? 0 }) : t(`sync.link.${ep.link}`);
  return (
    <div className="ep">
      <span className={`ep__dot ep__dot--${ep.link}`} aria-hidden />
      <span className="ep__text">
        <span className="ep__name">{ep.serverName} · {link}</span>
        <span className="ep__path mono" title={ep.remotePath}>{ep.remotePath}</span>
      </span>
      {(() => { const d = directionOf(localMode, [ep.mode]); return <Badge tone="neutral">{d === "mixed" ? t("syncEdit.mixedShort") : t(`sync.direction.${d}`)}</Badge>; })()}
      {onRemove && <IconButton size="sm" label={t("sync.removeServer", { name: ep.serverName })} title={t("sync.removeServer", { name: ep.serverName })} onClick={onRemove}><X size={13} /></IconButton>}
    </div>
  );
}

const eventIcon: Record<SyncEventView["kind"], ReactElement> = {
  down: <ArrowDownLeft size={13} aria-hidden />,
  up: <ArrowUpRight size={13} aria-hidden />,
  conflict: <TriangleAlert size={13} aria-hidden />,
  link: <Link2 size={13} aria-hidden />,
  error: <CircleAlert size={13} aria-hidden />,
  deleted: <Trash2 size={13} aria-hidden />,
};

/** 同步页上的操作（由容器实现） */
export interface SyncActions {
  onNew: () => void;
  /** 加载更早的事件 */
  onLoadMoreEvents?: (id: string) => void;
  onSetPaused: (id: string, paused: boolean) => void;
  onRescan: (id: string) => void;
  onRemove: (id: string) => void;
  onPauseAll: () => void;
  onResolveConflict: (taskId: string, conflict: SyncConflictView, keep: "current" | "other") => void;
  onBrowseVersions: (taskId: string) => void;
  /** 编辑任务设置（方向、协作模式、延迟、旧版本、冲突处理、排除规则） */
  onEdit: (taskId: string) => void;
  /** 本机只接收时，把本机改动还原成服务器上的版本 */
  onRevertLocal: (taskId: string) => void;
  /** 给任务再加一台服务器 */
  onAddServer?: (taskId: string) => void;
  /** 还有没加进这个任务的服务器吗 */
  canAddServer?: (taskId: string) => boolean;
  /** 从任务里移除一台服务器 */
  onRemoveServer?: (taskId: string, serverId: string) => void;
  /** 正在执行操作的任务（按钮显示为忙） */
  busyId: string | null;
}

function TaskDetail({ task, actions }: { task: SyncTaskView; actions?: SyncActions }) {
  const { t, i18n } = useTranslation();
  const n = (v: number) => formatCount(v, i18n.language);
  // 被删除保护自动暂停的任务也算暂停：按钮显示“继续”（与提示条里的按钮一致）
  const paused = task.state === "paused" || !!task.guardPaused;
  const multi = task.endpoints.length > 1;
  const ctx = useContextMenu(t("sync.moreActions"));
  // 这台电脑上的编辑器和这个任务默认用哪个（换任务时重新取，因为每个任务记住自己用的）
  const [editors, setEditors] = useState<{ editors: { id: string; name: string }[]; preferred: string | null } | null>(null);
  const [comparing, setComparing] = useState<SyncTaskView["conflicts"][number] | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  useEffect(() => {
    setEditors(null);
    void getSyncApi().then((api) => api.editors(task.id)).then(setEditors, () => setEditors({ editors: [], preferred: null }));
  }, [task.id]);
  const preferredEditor = editors?.editors.find((e) => e.id === editors.preferred) ?? null;
  const showNotice = (text: string) => { setNotice(text); setTimeout(() => setNotice((n) => (n === text ? null : n)), 8000); };
  const openEditor = (id: string | null) => {
    void getSyncApi().then((api) => api.openInEditor(task.id, id)).then(
      () => { if (id) setEditors((x) => (x ? { ...x, preferred: id } : x)); },
      (e: unknown) => showNotice(formatError(e)),
    );
  };
  const revealFolder = () => { void getFilesApi().then((api) => api.openDefault(task.localPath)).catch((e: unknown) => showNotice(formatError(e))); };
  const openSyncthingUi = () => { void getSyncApi().then((api) => api.openSyncthingUi()).catch((e: unknown) => showNotice(formatError(e))); };

  return (
    <section className="pl-panel sync-detail" aria-label={t("sync.detailLabel", { name: task.name })}>
      <header className="sync-detail__head">
        <h2>{task.name}</h2>
        <Badge tone={tone[task.state]}>{t(`sync.state.${task.state}`, { percent: task.percent, count: task.conflicts.length })}</Badge>
        <span className="sync-detail__spacer" />
        <Button size="sm" disabled={actions?.busyId === task.id} icon={paused ? <Play size={14} /> : <Pause size={14} />} onClick={() => actions?.onSetPaused(task.id, !paused)}>{paused ? t("sync.resume") : t("sync.pause")}</Button>
        <Button size="sm" disabled={actions?.busyId === task.id || paused} icon={<RefreshCw size={14} />} onClick={() => actions?.onRescan(task.id)}>{t("sync.rescan")}</Button>
        <Button size="sm" disabled={actions?.busyId === task.id} icon={<Pencil size={14} />} onClick={() => actions?.onEdit(task.id)}>{t("sync.editTask")}</Button>
        {/* 用编辑器打开：默认上次用的（没有就 VS Code）；下拉里换别的编辑器或在资源管理器里打开 */}
        <SplitButton
          disabled={editors === null}
          title={editors && !preferredEditor ? t("sync.noEditor") : undefined}
          label={preferredEditor ? t("sync.openIn", { editor: preferredEditor.name }) : t("sync.openFolder")}
          icon={<FolderOpen size={14} />}
          onClick={() => (preferredEditor ? openEditor(null) : revealFolder())}
          menuLabel={t("workspace.openWith")}
          moreLabel={t("workspace.chooseEditor")}
          entries={[
            ...(editors?.editors ?? []).map((e) => ({ id: `ed-${e.id}`, label: e.name, onSelect: () => openEditor(e.id) })),
            ...((editors?.editors.length ?? 0) > 0 ? [{ kind: "separator" as const, id: "sep" }] : []),
            { id: "folder", label: t("workspace.openFolder"), onSelect: revealFolder },
          ]}
        />
        <IconButton
          label={t("sync.moreActions")}
          size="sm"
          onClick={(e) => ctx.open(e, [
            { id: "remove", label: t("sync.removeTask"), danger: true, disabled: actions?.busyId === task.id, onSelect: () => actions?.onRemove(task.id) },
          ])}
        >
          <MoreHorizontal size={16} />
        </IconButton>
        {ctx.node}
      </header>

      <div className="sync-map">
        <div className="sync-map__side">
          <span className="sync-map__label">{t("sync.localSide")}</span>
          <span className="sync-map__path mono" title={task.localPath}>{task.localPath}</span>
        </div>
        {(() => {
          const d = directionOf(task.localMode, task.endpoints.map((e) => e.mode));
          const Icon = d === "toServer" ? ArrowRight : d === "toLocal" ? ArrowLeft : ArrowLeftRight;
          return <span className="sync-map__arrow" title={d === "mixed" ? undefined : t(`sync.direction.${d}`)}><Icon size={18} aria-hidden /></span>;
        })()}
        <div className="sync-map__eps">
          {task.endpoints.map((ep) => <EndpointRow key={ep.serverId} ep={ep} localMode={task.localMode} onRemove={task.endpoints.length > 1 && actions?.onRemoveServer ? () => actions.onRemoveServer?.(task.id, ep.serverId) : undefined} />)}
          {actions?.onAddServer && (
            <button type="button" className="sync-map__add" disabled={actions.busyId === task.id || actions.canAddServer?.(task.id) === false}
              title={actions.canAddServer?.(task.id) === false ? t("sync.noServerToAdd") : undefined} onClick={() => actions.onAddServer?.(task.id)}><Plus size={13} aria-hidden />{t("sync.addServer")}</button>
          )}
        </div>
      </div>

      <div className="sync-stats">
        <div><span>{t("sync.stat.files")}</span><b>{t("sync.stat.filesValue", { count: task.files, n: n(task.files), size: formatBytes(task.bytes) })}</b></div>
        <div><span>{t("sync.stat.excluded")}</span><b>{task.excludedFiles === null || task.excludedBytes === null ? t("sync.stat.rulesCount", { count: task.ignoreRules.length }) : t("sync.stat.filesValue", { count: task.excludedFiles, n: n(task.excludedFiles), size: formatBytes(task.excludedBytes) })}</b></div>
        <div><span>{t("sync.stat.versions")}</span><b>{task.versionsBytes === null ? t("common.off") : task.versionsBytes < 0 ? t("sync.versionsOn") : t("sync.stat.versionsValue", { size: formatBytes(task.versionsBytes) })}</b></div>
      </div>

      {task.loginServerId && (
        <Banner
          tone="warn"
          title={t("sync.needLoginTitle")}
          actions={<Button size="sm" variant="primary" onClick={() => task.loginServerId && void loginServer(task.loginServerId).catch(() => { /* 取消或失败：横幅还在，可以再点 */ })}>{t("servers.login")}</Button>}
        >
          {t("sync.needLoginBody")}
        </Banner>
      )}

      {task.passwordServerId && (
        <Banner
          tone="warn"
          title={t("sync.needPasswordTitle")}
          actions={<Button size="sm" variant="primary" onClick={() => task.passwordServerId && passwordBus.requestEntry(task.passwordServerId)}>{t("passwords.enter")}</Button>}
        >
          {t("sync.needPasswordBody")}
        </Banner>
      )}

      {task.localMode === "receiveOnly" && task.localChanges > 0 && (
        <Banner
          tone="warn"
          title={t("sync.localChangesTitle", { count: task.localChanges })}
          actions={<Button size="sm" disabled={actions?.busyId === task.id} onClick={() => actions?.onRevertLocal(task.id)}>{t("sync.revertLocal")}</Button>}
        >
          {t("sync.localChangesBody")}
        </Banner>
      )}

      {task.guardPaused && (
        <Banner
          tone="danger"
          title={t("sync.guardTitle")}
          actions={<>
            {task.versionsBytes !== null && <Button size="sm" onClick={() => actions?.onBrowseVersions(task.id)}>{t("sync.guardVersions")}</Button>}
            <Button size="sm" variant="primary" disabled={actions?.busyId === task.id} onClick={() => actions?.onSetPaused(task.id, false)}>{t("sync.guardResume")}</Button>
          </>}
        >
          {t("sync.guardBody")}
        </Banner>
      )}

      {task.errors && task.errors.length > 0 && (
        <div className="sync-conflicts sync-errors" role="region" aria-label={t("sync.errorsTitle")}>
          <div className="sync-conflicts__head">
            <CircleAlert size={15} aria-hidden />
            <b>{t("sync.errorsTitle")}</b>
            <span>{t("sync.errorsHint")}</span>
          </div>
          {task.errors.slice(0, 50).map((e) => (
            <div key={e.path} className="sync-conflict">
              <span className="sync-conflict__text">
                <span className="mono">{e.path}</span>
                <span className="sync-conflict__other">{e.reason}</span>
              </span>
            </div>
          ))}
        </div>
      )}

      {task.conflicts.length > 0 && (
        <div className="sync-conflicts" role="region" aria-label={t("sync.conflictsTitle")}>
          <div className="sync-conflicts__head">
            <TriangleAlert size={15} aria-hidden />
            <b>{t("sync.conflictsTitle")}</b>
            <span>{t("sync.conflictsHint")}</span>
          </div>
          {task.conflicts.map((c) => (
            <div key={c.path} className="sync-conflict">
              <span className="sync-conflict__text">
                <span className="mono">{c.path}</span>
                <span className="sync-conflict__other">{t("sync.conflictOther", { name: c.otherName, from: c.from })}</span>
              </span>
              <Button size="sm" onClick={() => setComparing(c)}>{t("sync.conflictCompare")}</Button>
              <Button size="sm" disabled={actions?.busyId === task.id} onClick={() => actions?.onResolveConflict(task.id, c, "current")}>{t("sync.conflictKeep")}</Button>
              <Button size="sm" disabled={actions?.busyId === task.id} onClick={() => actions?.onResolveConflict(task.id, c, "other")}>{t("sync.conflictUseOther")}</Button>
            </div>
          ))}
        </div>
      )}

      <div className="sync-grid">
        <div className="pl-card sync-rules">
          <div className="sync-card__head"><span className="ws__label">{t("sync.rules")}</span><Button size="sm" variant="ghost" onClick={() => actions?.onEdit(task.id)}>{t("sync.edit")}</Button></div>
          {task.ignoreRules.length === 0
            ? <span className="sync-muted">{t("sync.noRules")}</span>
            : (
              <div className="sync-rules__chips">
                {task.ignoreRules.map((r) => <span key={r} className={`rule-chip mono${isSensitiveRule(r) ? " rule-chip--sensitive" : ""}`}>{r.replace(/^\(\?d\)/, "")}</span>)}
              </div>
            )}
          <span className="sync-muted">{t("sync.rulesHint")}</span>
        </div>
        <div className="pl-card sync-settings">
          <span className="ws__label">{t("sync.settings")}</span>
          <dl>
            <dt>{t("newSync.mode")}</dt><dd>{(() => { const d = directionOf(task.localMode, task.endpoints.map((e) => e.mode)); return d === "mixed" ? t("syncEdit.mixedShort") : t(`sync.direction.${d}`); })()}</dd>
            <dt>{t("sync.collaboration")}</dt><dd>{task.collaboration ? t("common.on") : t("common.off")}</dd>
            <dt>{t("sync.watchDelay")}</dt><dd>{t("sync.watchDelayValue", { count: task.watchDelayS })}</dd>
            <dt>{t("sync.conflictMode")}</dt><dd>{t(`sync.conflictModes.${task.conflictMode}`)}</dd>
            <dt>{t("sync.versions")}</dt><dd>{task.versionsBytes === null ? t("common.off") : t("sync.versionsOn")}</dd>
          </dl>
          <div className="sync-settings__links">
            {task.versionsBytes !== null
              ? <button type="button" className="sync-link" onClick={() => actions?.onBrowseVersions(task.id)}>{t("sync.browseVersions")}</button>
              : <span className="sync-later" title={t("sync.versionsOffHint")}>{t("sync.browseVersions")}</span>}
            <button type="button" className="sync-link" onClick={openSyncthingUi}>{t("sync.openSyncthingUi")}</button>
          </div>
        </div>
      </div>

      <div className="sync-events">
        <span className="ws__label">{t("sync.events")}</span>
        {task.events.length === 0 && <span className="sync-muted">{t("sync.noEvents")}</span>}
        {task.events.map((e) => {
          const label = t(`sync.event.${e.kind}`);
          // 悬停提示只挂在文字本身上（文字列按内容宽度，右边空白处不弹）
          return (
            <div key={e.id} className={`sync-event sync-event--${e.kind}`}>
              <span className="sync-event__time mono">{e.time}</span>
              <span className="sync-event__kind">{eventIcon[e.kind]}{label}</span>
              {/* 谁改的：本机或哪台服务器；不知道时留空占位，各行对齐 */}
              <span className="sync-event__by pl-ellipsis" title={e.by ? t("sync.byTitle", { who: e.by }) : undefined}>{e.by ?? ""}</span>
              {multi && <span className="sync-event__server" title={e.server ?? undefined}>{e.server && e.server !== e.by && <><Server size={11} aria-hidden /><span className="pl-ellipsis">{e.server}</span></>}</span>}
              <span className="sync-event__text mono" title={e.text}>{e.text}</span>
            </div>
          );
        })}
        {task.eventsHasMore && (
          <Button size="sm" variant="ghost" className="sync-events__more" onClick={() => actions?.onLoadMoreEvents?.(task.id)}>{t("sync.loadMoreEvents")}</Button>
        )}
      </div>
      {comparing && (
        <ConflictDiffDialog
          taskId={task.id}
          path={comparing.path}
          conflictFile={(comparing.record as { conflictFile?: string } | undefined)?.conflictFile ?? comparing.otherName}
          onKeep={(keep) => { const c = comparing; setComparing(null); actions?.onResolveConflict(task.id, c, keep); }}
          onClose={() => setComparing(null)}
        />
      )}
      {/* 打开编辑器或管理页失败：浮在底部提示区，不挤动页面 */}
      {notice && <Notice order={0}><div className="ws-load-error"><Banner tone="danger" title={notice} actions={<Button size="sm" variant="ghost" onClick={() => setNotice(null)}>{t("mcp.gotIt")}</Button>} /></div></Notice>}
    </section>
  );
}

/** 同步页：左边任务列表，右边选中任务的详情 */
export function SyncPage({ tasks, actions }: { tasks: SyncTaskView[]; actions?: SyncActions }) {
  const { t } = useTranslation();
  const [filter, setFilter] = useState<Filter>("all");
  const [selectedId, setSelectedId] = useState<string | null>(tasks[0]?.id ?? null);
  const shown = useMemo(() => tasks.filter((x) => matches(x, filter)), [tasks, filter]);
  // 没选中（或选中的任务已被移除）时默认显示第一个
  const selected = tasks.find((x) => x.id === selectedId) ?? tasks[0] ?? null;
  const count = (f: Filter) => tasks.filter((x) => matches(x, f)).length;

  return (
    <div className="sync-page">
      <section className="pl-panel sync-list" aria-label={t("sync.listLabel")}>
        <div className="sync-list__head">
          <h1>{t("sync.title")}</h1>
          <span className="sync-detail__spacer" />
          <Button size="sm" variant="ghost" icon={<Pause size={14} />} disabled={!tasks.some((x) => x.state !== "paused")} onClick={actions?.onPauseAll}>{t("sync.pauseAll")}</Button>
          <Button size="sm" variant="primary" icon={<Plus size={14} />} onClick={actions?.onNew}>{t("sync.newTask")}</Button>
        </div>
        <Segmented<Filter>
          label={t("sync.filterLabel")}
          value={filter}
          onChange={setFilter}
          options={(["all", "syncing", "conflict", "error", "paused"] as Filter[]).map((f) => ({ value: f, label: `${t(`sync.filter.${f}`)} ${count(f)}` }))}
        />
        <div className="sync-list__items">
          {shown.map((x) => <TaskRow key={x.id} task={x} selected={x.id === selected?.id} onSelect={() => setSelectedId(x.id)} />)}
          {shown.length === 0 && <span className="sync-muted sync-list__empty">{t("sync.emptyFilter")}</span>}
        </div>
      </section>
      {selected
        ? <TaskDetail task={selected} actions={actions} />
        : (
          <section className="pl-panel sync-empty">
            <b>{t("sync.emptyTitle")}</b>
            <span>{t("sync.emptyBody")}</span>
            <Button variant="primary" icon={<Plus size={14} />} onClick={actions?.onNew}>{t("sync.newTask")}</Button>
          </section>
        )}
    </div>
  );
}
