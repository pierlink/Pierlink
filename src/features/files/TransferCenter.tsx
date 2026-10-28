import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { useTranslation } from "react-i18next";
import { createPortal } from "react-dom";
import { ArrowDownUp, ArrowRightLeft, Check, CircleAlert, X } from "lucide-react";
import { Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { getSyncApi } from "../sync/syncApi";
import { getFilesApi } from "./api";
import type { DragPayload, DropTarget } from "./dnd";
import { useDragState, dropTargetInfo } from "./dnd";
import { formatSize, joinPath, sameLoc, syncCovering, type SyncPair } from "./path";
import { cancelTransfer, clearFinishedTransfers, dismissTransfer, issuesOf, jobFailed, runningSummary, startTransfer, useTransfers, type TransferJob } from "./transfers";
import type { TransferPlanView, TransferRequest } from "./types";

/** 放下文件后的确认步骤 */
type Step =
  | { kind: "sameMachine" }
  | { kind: "synced"; task: string; p: DragPayload; t: DropTarget }
  | { kind: "planning"; req: TransferRequest }
  | { kind: "conflict"; plan: TransferPlanView; req: TransferRequest; label: string }
  | { kind: "error"; text: string };

let pendingDrop: { p: DragPayload; t: DropTarget; n: number } | null = null;
const dropListeners = new Set<() => void>();

/** 文件面板收到放下的文件时调用：交给传输中心按步骤确认后开始传输 */
export function requestDrop(p: DragPayload, t: DropTarget): void {
  pendingDrop = { p, t, n: (pendingDrop?.n ?? 0) + 1 };
  dropListeners.forEach((l) => l());
}

/** 读取已同步的目录对（失败时当作没有同步任务，不阻止传输） */
async function syncPairs(): Promise<SyncPair[]> {
  try {
    const tasks = await (await getSyncApi()).tasks();
    return tasks.flatMap((x) => x.endpoints.map((e) => ({ name: x.name, localPath: x.localPath, serverId: e.serverId, remotePath: e.remotePath })));
  } catch {
    return [];
  }
}

/**
 * 传输中心：处理拖放后的确认（同一台机器、已同步目录、同名冲突），
 * 还负责显示拖动时跟随鼠标的提示。传输进度在标题栏的“传输”按钮里（TransferButton）。
 */
export function TransferCenter() {
  const { t } = useTranslation();
  const drop = useSyncExternalStore((l) => { dropListeners.add(l); return () => dropListeners.delete(l); }, () => pendingDrop);
  const [step, setStep] = useState<Step | null>(null);

  const labelFor = (p: DragPayload, target: DropTarget) =>
    t("files.transferLabel", { what: p.names.length === 1 ? p.names[0] : t("files.items", { count: p.names.length }), to: target.title, dir: target.dir });

  /** 统计并检查同名项，没有冲突就直接开始 */
  // 每次统计加一；统计期间点了取消（或开始了新的拖放），迟到的结果据此丢弃，不会再开始传输
  const planSeq = useRef(0);
  const close = () => { planSeq.current++; setStep(null); };
  const plan = async (p: DragPayload, target: DropTarget) => {
    const my = ++planSeq.current;
    const req: TransferRequest = { from: p.loc, sources: p.names.map((n) => joinPath(p.dir, n)), to: target.loc, destDir: target.dir };
    const label = labelFor(p, target);
    setStep({ kind: "planning", req });
    try {
      const result = await (await getFilesApi()).plan(req);
      if (my !== planSeq.current) return;
      if (result.conflicts.length) { setStep({ kind: "conflict", plan: result, req, label }); return; }
      setStep(null);
      void startTransfer(req, "error", label);
    } catch (e) {
      if (my === planSeq.current) setStep({ kind: "error", text: formatError(e) });
    }
  };

  useEffect(() => {
    if (!drop) return;
    planSeq.current++;
    const { p, t: target } = drop;
    if (sameLoc(p.loc, target.loc)) { setStep({ kind: "sameMachine" }); return; }
    void (async () => {
      const home = p.loc.kind === "server" ? p.home : target.home;
      const task = syncCovering(await syncPairs(), { loc: p.loc, dir: p.dir }, { loc: target.loc, dir: target.dir }, home);
      if (task) setStep({ kind: "synced", task, p, t: target });
      else void plan(p, target);
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [drop]);

  return (
    <>
      <DragGhost />
      {step?.kind === "sameMachine" && (
        <Dialog title={t("files.sameMachineTitle")} onDismiss={() => setStep(null)} actions={<Button variant="primary" onClick={() => setStep(null)}>{t("common.ok")}</Button>}>
          {t("files.sameMachineBody")}
        </Dialog>
      )}
      {step?.kind === "synced" && (
        <Dialog
          title={t("files.syncedTitle")}
          onDismiss={() => setStep(null)}
          actions={<>
            <Button onClick={() => { const s = step; setStep(null); void plan(s.p, s.t); }}>{t("files.transferAnyway")}</Button>
            <Button variant="primary" onClick={() => setStep(null)}>{t("common.cancel")}</Button>
          </>}
        >
          {t("files.syncedBody", { task: step.task })}
        </Dialog>
      )}
      {step?.kind === "planning" && (
        <Dialog title={t("files.planning")} onDismiss={close} actions={<Button onClick={close}>{t("common.cancel")}</Button>}>
          <span role="status">{t("files.planningBody")}</span>
        </Dialog>
      )}
      {step?.kind === "conflict" && (
        <Dialog
          title={t("files.conflictTitle", { count: step.plan.conflicts.length })}
          onDismiss={() => setStep(null)}
          actions={<>
            <Button onClick={() => setStep(null)}>{t("common.cancel")}</Button>
            <Button onClick={() => { const s = step; setStep(null); void startTransfer(s.req, "skip", s.label); }}>{t("files.skipExisting")}</Button>
            <Button variant="danger" onClick={() => { const s = step; setStep(null); void startTransfer(s.req, "overwrite", s.label); }}>{t("files.overwrite")}</Button>
          </>}
        >
          <p style={{ marginTop: 0 }}>{t("files.conflictBody", { files: step.plan.files, size: formatSize(step.plan.bytes) })}</p>
          <ul className="xfer-conflicts mono">
            {step.plan.conflicts.slice(0, 50).map((c) => <li key={c}>{c}</li>)}
            {step.plan.conflicts.length > 50 && <li>{t("files.andMore", { count: step.plan.conflicts.length - 50 })}</li>}
          </ul>
        </Dialog>
      )}
      {step?.kind === "error" && (
        <Dialog title={t("files.planFailed")} danger onDismiss={() => setStep(null)} actions={<Button variant="primary" onClick={() => setStep(null)}>{t("common.ok")}</Button>}>
          <span role="alert" style={{ overflowWrap: "anywhere" }}>{step.text}</span>
        </Dialog>
      )}
    </>
  );
}

/**
 * 标题栏右上角的“传输”按钮：一直显示。传输进行中显示个数和总进度；有失败时变红。
 * 点开是传输列表（本次运行内的所有传输，包括已经结束的），可以取消、看详情、清除已结束的。
 */
export function TransferButton() {
  const { t } = useTranslation();
  const jobs = useTransfers();
  const [open, setOpen] = useState(false);
  /** 上次打开列表时已经结束的任务（之后结束的失败任务才算“没看过”） */
  const [seen, setSeen] = useState(0);
  const btn = useRef<HTMLButtonElement>(null);
  const pop = useRef<HTMLDivElement>(null);
  const { count, percent } = runningSummary(jobs);
  const unseenBad = jobs.some((j) => jobFailed(j) && (j.finishedAt ?? 0) > seen);
  const lastStart = jobs.length ? jobs[jobs.length - 1].key : 0;
  // 新开始一个传输：按钮闪一下，提示进度在这里
  const [flash, setFlash] = useState(false);
  useEffect(() => {
    if (!lastStart) return;
    setFlash(true);
    const timer = setTimeout(() => setFlash(false), 1600);
    return () => clearTimeout(timer);
  }, [lastStart]);
  useEffect(() => {
    if (!open) return;
    setSeen(Date.now());
    const onDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (!pop.current?.contains(target) && !btn.current?.contains(target)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") setOpen(false); };
    window.addEventListener("mousedown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("mousedown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open, jobs.length]);
  const ended = jobs.some((j) => j.state !== "running");
  const r = btn.current?.getBoundingClientRect();

  return (
    <>
      <button
        ref={btn}
        type="button"
        className={`xfer-btn${count ? " xfer-btn--busy" : ""}${unseenBad ? " xfer-btn--bad" : ""}${flash ? " xfer-btn--flash" : ""}`}
        aria-expanded={open}
        aria-haspopup="dialog"
        title={t("files.transfersTitle")}
        aria-label={t("files.transfers")}
        onClick={() => setOpen(!open)}
      >
        <ArrowDownUp size={14} />
        <span className="xfer-btn__text">{count > 0 ? t("files.trayRunning", { count, percent }) : t("files.transfers")}</span>
        {count > 0 && <span className="xfer-btn__bar"><span style={{ width: `${percent}%` }} /></span>}
        {unseenBad && <span className="xfer-btn__dot" aria-hidden="true" />}
      </button>
      {open && r && createPortal(
        <div ref={pop} className="xfer-pop" role="dialog" aria-label={t("files.transfers")} style={{ top: r.bottom + 6, right: Math.max(8, window.innerWidth - r.right) }}>
          <div className="xfer-pop__head">
            <span className="xfer-pop__title">{t("files.transfers")}</span>
            {ended && <button type="button" className="xfer-pop__clear" onClick={clearFinishedTransfers}>{t("files.clearFinished")}</button>}
          </div>
          {jobs.length === 0
            ? <div className="xfer-pop__empty">{t("files.noTransfers")}</div>
            : <div className="xfer-pop__list">{[...jobs].reverse().map((j) => <JobRow key={j.key} job={j} />)}</div>}
        </div>,
        document.body,
      )}
    </>
  );
}

/** 一个传输任务：进度条、当前文件、取消；结束后显示结果 */
function JobRow({ job }: { job: TransferJob }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const p = job.progress;
  const percent = p && p.totalBytes > 0 ? Math.min(100, Math.round((p.doneBytes / p.totalBytes) * 100)) : 0;
  const r = job.report;
  const issues = r ? issuesOf(job) : 0;

  return (
    <div className={`xfer__job xfer__job--${job.state}`}>
      <div className="xfer__head">
        {job.state === "running" ? <ArrowRightLeft size={14} className="xfer__icon" /> : job.state === "done" && !r?.failed.length ? <Check size={14} className="xfer__icon xfer__icon--ok" /> : <CircleAlert size={14} className="xfer__icon xfer__icon--bad" />}
        <span className="xfer__label pl-ellipsis" title={job.label}>{job.label}</span>
        {job.state === "running"
          ? <button type="button" className="xfer__x" aria-label={t("common.cancel")} title={t("common.cancel")} disabled={job.cancelRequested} onClick={() => void cancelTransfer(job.key)}><X size={13} /></button>
          : <button type="button" className="xfer__x" aria-label={t("common.close")} title={t("common.close")} onClick={() => dismissTransfer(job.key)}><X size={13} /></button>}
      </div>
      {job.state === "running" && (
        <>
          <div className="xfer__bar" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent}><span style={{ width: `${percent}%` }} /></div>
          <div className="xfer__meta pl-ellipsis">
            {job.cancelRequested ? t("files.cancelling") : p ? `${formatSize(p.doneBytes)} / ${formatSize(p.totalBytes)} · ${p.filesDone}/${p.filesTotal} · ${p.current}` : t("files.starting")}
          </div>
        </>
      )}
      {job.state === "cancelled" && <div className="xfer__meta">{t("files.cancelled")}</div>}
      {job.state === "failed" && <div className="xfer__meta xfer__meta--bad">{job.error}</div>}
      {job.state === "done" && r && (
        <div className="xfer__meta">
          {t("files.doneSummary", { count: r.done })}
          {r.skipped.length > 0 && ` · ${t("files.skippedCount", { count: r.skipped.length })}`}
          {r.renamed.length > 0 && ` · ${t("files.renamedCount", { count: r.renamed.length })}`}
          {r.failed.length > 0 && <span className="xfer__meta--bad">{` · ${t("files.failedCount", { count: r.failed.length })}`}</span>}
          {issues > 0 && <button type="button" className="xfer__more" onClick={() => setOpen(!open)}>{open ? t("files.hideDetails") : t("files.details")}</button>}
        </div>
      )}
      {open && r && (
        <ul className="xfer__details mono">
          {r.failed.map((f) => <li key={`f-${f.path}`} className="xfer__meta--bad">{f.path}: {f.reason}</li>)}
          {r.renamed.map((x) => <li key={`r-${x.from}`}>{x.from} → {x.to}</li>)}
          {r.skipped.map((s) => <li key={`s-${s}`}>{t("files.skippedItem", { name: s })}</li>)}
        </ul>
      )}
    </div>
  );
}

/** 拖动文件时跟随鼠标的提示：拖到可以放的面板上时显示目标 */
function DragGhost() {
  const { t } = useTranslation();
  const drag = useDragState();
  if (!drag) return null;
  const target = dropTargetInfo(drag.over, drag.payload);
  const what = drag.payload.names.length === 1 ? drag.payload.names[0] : t("files.items", { count: drag.payload.names.length });
  return (
    <div className={`drag-ghost${target ? " drag-ghost--ok" : ""}`} style={{ left: drag.x + 14, top: drag.y + 14 }} aria-hidden="true">
      <span className="pl-ellipsis">{what}</span>
      {target && <span className="drag-ghost__to pl-ellipsis">→ {target.title}</span>}
    </div>
  );
}
