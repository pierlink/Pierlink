import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Dialog, Notice } from "../../components";
import { formatError } from "../../errors";
import { useSettings } from "../settings/store";
import { getServersApi, type ServerRecord } from "../servers/api";
import { NewSyncWizard } from "./NewSyncWizard";
import { SyncPage } from "./SyncPage";
import { getSyncApi } from "./syncApi";
import { useSyncTasks } from "./useSyncTasks";
import { VersionsDialog } from "./VersionsDialog";
import { ConfigReportHost } from "./ConfigReportDialog";
import { EditTaskDialog } from "./EditTaskDialog";
import { RevertDialog } from "./RevertDialog";
import { DeployFlow } from "./DeployFlow";
import type { SyncTaskView } from "./types";

type Modal =
  | { kind: "new" }
  | { kind: "addServer"; task: SyncTaskView }
  | { kind: "removeServer"; task: SyncTaskView; serverId: string; busy: boolean; error: string | null }
  | { kind: "remove"; task: SyncTaskView; busy: boolean; error: string | null }
  | { kind: "error"; title: string; text: string }
  | { kind: "versions"; taskId: string; name: string }
  | { kind: "deploy"; serverId: string; name: string }
  | { kind: "edit"; taskId: string }
  | { kind: "revert"; taskId: string };

/** 同步页容器：数据、操作、新建向导与确认框 */
export function SyncScreen() {
  const { t } = useTranslation();
  const { settings } = useSettings();
  const { tasks, loadError, reload, loadMoreEvents, refreshConflicts } = useSyncTasks();
  const [servers, setServers] = useState<ServerRecord[]>([]);
  const [deployed, setDeployed] = useState<string[]>([]);
  const [modal, setModal] = useState<Modal | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);

  const loadServers = async () => {
    const [list, dep] = await Promise.all([(await getServersApi()).list(), (await getSyncApi()).deployedServers()]);
    setServers(list);
    setDeployed(dep);
  };
  useEffect(() => { void loadServers().catch(() => { /* 服务器列表读取失败时向导里没有可选服务器 */ }); }, []);

  /** 执行一个任务操作：期间按钮显示为忙，失败弹出原因 */
  const run = async (id: string | null, title: string, fn: () => Promise<void>) => {
    setBusyId(id);
    try {
      await fn();
      await reload();
    } catch (e) {
      setModal({ kind: "error", title, text: formatError(e) });
    } finally {
      setBusyId(null);
    }
  };

  return (
    <>
      <SyncPage
        tasks={tasks}
        actions={{
          busyId,
          onLoadMoreEvents: (id) => void loadMoreEvents(id).catch((e: unknown) => setModal({ kind: "error", title: t("sync.loadMoreFailed"), text: formatError(e) })),
          onNew: () => { void loadServers(); setModal({ kind: "new" }); },
          onSetPaused: (id, paused) => void run(id, paused ? t("sync.pauseFailed") : t("sync.resumeFailed"), async () => (await getSyncApi()).setPaused(id, paused)),
          onRescan: (id) => void run(id, t("sync.rescanFailed"), async () => (await getSyncApi()).rescan(id)),
          onRemove: (id) => { const task = tasks.find((x) => x.id === id); if (task) setModal({ kind: "remove", task, busy: false, error: null }); },
          onResolveConflict: (id, c, keep) => void run(id, t("sync.resolveFailed"), async () => {
            await (await getSyncApi()).resolveConflict(id, c.record as never, keep);
            refreshConflicts();
          }),
          onBrowseVersions: (id) => { const task = tasks.find((x) => x.id === id); if (task) setModal({ kind: "versions", taskId: id, name: task.name }); },
          onEdit: (id) => setModal({ kind: "edit", taskId: id }),
          onRevertLocal: (id) => setModal({ kind: "revert", taskId: id }),
          canAddServer: (id) => { const task = tasks.find((x) => x.id === id); return !!task && servers.some((s) => !task.endpoints.some((e) => e.serverId === s.id)); },
          onAddServer: (id) => { const task = tasks.find((x) => x.id === id); if (task) { void loadServers(); setModal({ kind: "addServer", task }); } },
          onRemoveServer: (id, serverId) => { const task = tasks.find((x) => x.id === id); if (task) setModal({ kind: "removeServer", task, serverId, busy: false, error: null }); },
          onPauseAll: () => void run(null, t("sync.pauseFailed"), async () => {
            const api = await getSyncApi();
            for (const x of tasks.filter((x) => x.state !== "paused")) await api.setPaused(x.id, true);
          }),
        }}
      />
      {loadError !== null && <Notice order={0}><div className="ws-load-error"><Banner tone="danger" title={t("sync.loadFailed")}>{formatError(loadError)}</Banner></div></Notice>}

      {modal?.kind === "addServer" && (
        <NewSyncWizard
          addTo={{ name: modal.task.name, localPath: modal.task.localPath, localMode: modal.task.localMode, rules: modal.task.ignoreRules }}
          servers={servers.filter((s) => !modal.task.endpoints.some((e) => e.serverId === s.id)).map((s) => ({ id: s.id, name: s.name, deployed: deployed.includes(s.id) }))}
          defaultRules={modal.task.ignoreRules}
          checkPaths={async (d) => (await getSyncApi()).checkPaths(d)}
          estimate={async (d, onProgress) => (await getSyncApi()).estimate(d, onProgress)}
          onCancelEstimate={() => void getSyncApi().then((api) => api.cancelEstimate())}
          onCreate={async (d, start) => {
            try {
              await (await getSyncApi()).addEndpoint(modal.task.id, d.serverId, d.remotePath, d.mode, start);
              setModal(null);
            } catch (e) {
              if ((e as { code?: string } | null)?.code !== "sync.created_not_started") throw e;
              setModal({ kind: "error", title: t("sync.createdNotStartedTitle"), text: t("sync.createdNotStartedBody") });
            }
            await reload();
          }}
          onDeploy={(serverId) => {
            const s = servers.find((x) => x.id === serverId);
            setModal({ kind: "deploy", serverId, name: s?.name ?? "" });
          }}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.kind === "removeServer" && (() => {
        const ep = modal.task.endpoints.find((e) => e.serverId === modal.serverId);
        const remove = async () => {
          setModal({ ...modal, busy: true, error: null });
          try {
            await (await getSyncApi()).removeEndpoint(modal.task.id, modal.serverId);
            setModal(null);
            await reload();
          } catch (e) {
            setModal({ ...modal, busy: false, error: formatError(e) });
          }
        };
        return (
          <Dialog
            title={t("sync.removeServerTitle", { name: ep?.serverName ?? "", task: modal.task.name })}
            onDismiss={modal.busy ? undefined : () => setModal(null)}
            actions={<>
              <Button disabled={modal.busy} onClick={() => setModal(null)}>{t("common.cancel")}</Button>
              <Button variant="danger" disabled={modal.busy} onClick={() => void remove()}>{t("sync.removeServerConfirm")}</Button>
            </>}
          >
            <p>{t("sync.removeServerBody", { name: ep?.serverName ?? "", path: ep?.remotePath ?? "" })}</p>
            {modal.error && <Banner tone="danger" title={t("sync.removeServerFailed")}>{modal.error}</Banner>}
          </Dialog>
        );
      })()}
      {modal?.kind === "new" && (
        <NewSyncWizard
          servers={servers.map((s) => ({ id: s.id, name: s.name, deployed: deployed.includes(s.id) }))}
          defaultRules={settings.syncDefaults.ignoreRules}
          defaultConflictMode={settings.syncDefaults.conflictMode}
          checkPaths={async (d) => (await getSyncApi()).checkPaths(d)}
          estimate={async (d, onProgress) => (await getSyncApi()).estimate(d, onProgress)}
          onCancelEstimate={() => void getSyncApi().then((api) => api.cancelEstimate())}
          onCreate={async (d, start) => {
            try {
              await (await getSyncApi()).create(d, start, settings.syncDefaults.watchDelayS, settings.syncDefaults.keepVersions);
              setModal(null);
            } catch (e) {
              // 任务已建好但没能开始：关闭向导，告诉用户可以稍后在任务页点“继续同步”
              if ((e as { code?: string } | null)?.code !== "sync.created_not_started") throw e;
              setModal({ kind: "error", title: t("sync.createdNotStartedTitle"), text: t("sync.createdNotStartedBody") });
            }
            await reload();
          }}
          onDeploy={(serverId) => {
            const s = servers.find((x) => x.id === serverId);
            setModal({ kind: "deploy", serverId, name: s?.name ?? "" });
          }}
          onClose={() => setModal(null)}
        />
      )}
      {modal?.kind === "remove" && (
        <Dialog
          title={t("sync.removeTitle", { name: modal.task.name })}
          danger
          onDismiss={modal.busy ? undefined : () => setModal(null)}
          actions={<>
            <Button onClick={() => setModal(null)} disabled={modal.busy}>{t("common.cancel")}</Button>
            <Button variant="danger" disabled={modal.busy} onClick={() => {
              const m = modal;
              setModal({ ...m, busy: true, error: null });
              void getSyncApi().then((api) => api.remove(m.task.id)).then(
                () => { setModal(null); void reload(); },
                (e: unknown) => setModal({ ...m, busy: false, error: formatError(e) }),
              );
            }}>{modal.busy ? t("sync.removing") : t("sync.removeTask")}</Button>
          </>}
        >
          {t("sync.removeBody", { local: modal.task.localPath, remote: modal.task.endpoints.map((e) => `${e.serverName}:${e.remotePath}`).join(t("common.listSep")) })}
          {modal.error && <div style={{ marginTop: 10 }}><Banner tone="danger" title={t("sync.removeFailed")}>{modal.error}</Banner></div>}
        </Dialog>
      )}
      {modal?.kind === "deploy" && (
        <DeployFlow serverId={modal.serverId} serverName={modal.name} onClose={() => { setModal(null); void loadServers(); }} />
      )}
      {modal?.kind === "edit" && (() => {
        const task = tasks.find((x) => x.id === modal.taskId);
        if (!task) return null;
        return (
          <EditTaskDialog
            task={task}
            versioning={task.versionsBytes !== null}
            defaultDelayS={settings.syncDefaults.watchDelayS}
            onSave={async (st) => { await (await getSyncApi()).updateSettings(task.id, st); await reload(); }}
            onRevert={async () => { await (await getSyncApi()).revertLocal(task.id); await reload(); }}
            onClose={() => setModal(null)}
          />
        );
      })()}
      {modal?.kind === "revert" && (() => {
        const task = tasks.find((x) => x.id === modal.taskId);
        if (!task) return null;
        return (
          <RevertDialog
            count={task.localChanges}
            versioning={task.versionsBytes !== null}
            onConfirm={async () => { await (await getSyncApi()).revertLocal(task.id); await reload(); }}
            onClose={() => setModal(null)}
          />
        );
      })()}
      <ConfigReportHost />
      {modal?.kind === "versions" && <VersionsDialog taskId={modal.taskId} taskName={modal.name} onClose={() => setModal(null)} />}
      {modal?.kind === "error" && (
        <Dialog title={modal.title} danger onDismiss={() => setModal(null)} actions={<Button variant="primary" onClick={() => setModal(null)}>{t("common.ok")}</Button>}>
          {modal.text}
        </Dialog>
      )}
    </>
  );
}
