import { useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowLeft, ArrowLeftRight, ArrowRight } from "lucide-react";
import { Banner, Button, Dialog, Select, Switch } from "../../components";
import { formatError } from "../../errors";
import { directionOf, modesOf, type Direction } from "./direction";
import type { TaskSettingsInput } from "./syncApi";
import type { SyncTaskView } from "./types";
import { RevertDialog } from "./RevertDialog";

const DIRS: Direction[] = ["both", "toServer", "toLocal"];
const dirIcon = { both: ArrowLeftRight, toServer: ArrowRight, toLocal: ArrowLeft } as const;

/**
 * 编辑同步任务：方向、协作模式、改动后多久同步、保留旧版本、冲突处理、排除规则。
 * 保存时两端的 Syncthing 配置一起改；任一端失败（如服务器离线）都不改，提示原因。
 */
export function EditTaskDialog({ task, versioning, defaultDelayS, onSave, onRevert, onClose }: {
  task: SyncTaskView;
  /** 设置里的默认延迟：协作任务（延迟固定 1 秒）关掉协作时用它 */
  defaultDelayS: number;
  /** 当前是否保留旧版本 */
  versioning: boolean;
  onSave: (s: TaskSettingsInput) => Promise<void>;
  /** 本机只接收时，把本机改动还原成服务器上的版本 */
  onRevert: () => Promise<void>;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const current = directionOf(task.localMode, task.endpoints.map((e) => e.mode));
  const [dir, setDir] = useState<Direction | "mixed">(current);
  const [collab, setCollab] = useState(task.collaboration);
  // 协作任务的延迟固定是 1 秒，关掉协作时换成设置里的默认值
  const [delay, setDelay] = useState(String(task.collaboration && task.watchDelayS <= 1 ? defaultDelayS : task.watchDelayS));
  const [confirmRevert, setConfirmRevert] = useState(false);
  const [keep, setKeep] = useState(versioning);
  const [conflict, setConflict] = useState(task.conflictMode);
  const [rules, setRules] = useState(task.ignoreRules.join("\n"));
  const [busy, setBusy] = useState<"save" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const delayN = Number(delay);
  const delayOk = collab || (Number.isInteger(delayN) && delayN >= 1 && delayN <= 60);

  const save = async () => {
    if (!delayOk) return;
    setBusy("save");
    setError(null);
    // 方向没改（含“各服务器不同”）时保留每台服务器原来的方式
    const modes = dir === "mixed" || dir === current ? null : modesOf(dir);
    try {
      await onSave({
        localMode: modes ? modes.localMode : task.localMode,
        endpoints: task.endpoints.map((e) => ({ serverId: e.serverId, mode: modes ? modes.serverMode : e.mode })),
        collaboration: collab,
        watchDelayS: collab ? 1 : delayN,
        versioning: keep,
        conflictMode: conflict,
        rules: rules.split("\n").map((r) => r.trim()).filter(Boolean),
      });
      onClose();
    } catch (e) {
      setError(formatError(e));
      setBusy(null);
    }
  };
  return (
    <Dialog
      title={t("syncEdit.title", { name: task.name })}
      size="lg"
      onDismiss={busy || confirmRevert ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={!!busy}>{t("common.cancel")}</Button>
        <Button variant="primary" disabled={!!busy || !delayOk} onClick={() => void save()}>{busy === "save" ? t("syncEdit.saving") : t("common.save")}</Button>
      </>}
    >
      <div className="sync-edit">
        <div className="wiz-field">
          <span className="wiz-field__label">{t("newSync.mode")}</span>
          <div className="sync-edit__dirs" role="radiogroup" aria-label={t("newSync.mode")}>
            {DIRS.map((d) => {
              const Icon = dirIcon[d];
              return (
                <button key={d} type="button" role="radio" aria-checked={dir === d} className="sync-edit__dir" onClick={() => setDir(d)}>
                  <span className="sync-edit__dir-head"><Icon size={14} aria-hidden />{t(`sync.direction.${d}`)}</span>
                  <span className="sync-edit__dir-body">{t(`syncEdit.dirHint.${d}`)}</span>
                </button>
              );
            })}
          </div>
          {dir === "mixed" && <span className="wiz-hint">{t("syncEdit.mixed")}</span>}
          {task.endpoints.length > 1 && dir !== "mixed" && dir !== current && <span className="wiz-hint">{t("syncEdit.allServers", { count: task.endpoints.length })}</span>}
        </div>

        {task.localMode === "receiveOnly" && task.localChanges > 0 && (
          <Banner
            tone="warn"
            title={t("sync.localChangesTitle", { count: task.localChanges })}
            actions={<Button size="sm" disabled={!!busy} onClick={() => setConfirmRevert(true)}>{t("sync.revertLocal")}</Button>}
          >
            {t("sync.localChangesBody")}
          </Banner>
        )}

        <div className="sync-edit__rows">
          <div className="sync-edit__row">
            <span className="sync-edit__text"><span>{t("sync.collaboration")}</span><span className="wiz-hint">{t("syncEdit.collabHint")}</span></span>
            <Switch label={t("sync.collaboration")} checked={collab} onChange={setCollab} />
          </div>
          <div className="sync-edit__row">
            <span className="sync-edit__text"><span>{t("syncEdit.delay")}</span><span className="wiz-hint">{collab ? t("syncEdit.delayCollab") : t("syncEdit.delayHint")}</span></span>
            <span className="sync-edit__delay">
              <input className="pl-input mono" inputMode="numeric" aria-label={t("syncEdit.delay")} aria-invalid={!delayOk} disabled={collab} value={collab ? "1" : delay} onChange={(e) => setDelay(e.target.value.replace(/[^0-9]/g, "").slice(0, 2))} />
              {t("syncEdit.seconds")}
            </span>
          </div>
          <div className="sync-edit__row">
            <span className="sync-edit__text"><span>{t("sync.versions")}</span><span className="wiz-hint">{t("syncEdit.versionsHint")}</span></span>
            <Switch label={t("sync.versions")} checked={keep} onChange={setKeep} />
          </div>
          <div className="sync-edit__row">
            <span className="sync-edit__text"><span>{t("sync.conflictMode")}</span></span>
            <Select label={t("sync.conflictMode")} value={conflict} onChange={(v) => setConflict(v as "ask" | "newest")} options={[{ value: "ask", label: t("sync.conflictModes.ask") }, { value: "newest", label: t("sync.conflictModes.newest") }]} />
          </div>
        </div>

        <label className="wiz-field">
          <span className="wiz-field__label">{t("newSync.rulesLabel", { count: rules.split("\n").filter((r) => r.trim()).length })}</span>
          <textarea className="pl-input mono wiz-rules" spellCheck={false} rows={7} value={rules} onChange={(e) => setRules(e.target.value)} />
          <span className="wiz-hint">{t("syncEdit.rulesHint")}</span>
        </label>

        {task.localMode === "receiveOnly" && task.localChanges > 0 && dir !== "mixed" && dir !== "toLocal" && (
          <Banner tone="danger" title={t("sync.dirAwayWarnTitle", { count: task.localChanges })}>{t("sync.dirAwayWarnBody")}</Banner>
        )}
        {error && <Banner tone="danger" title={t("syncEdit.failed")}>{error}</Banner>}
      </div>
      {confirmRevert && (
        <RevertDialog count={task.localChanges} versioning={versioning} onConfirm={onRevert} onClose={() => setConfirmRevert(false)} />
      )}
    </Dialog>
  );
}
