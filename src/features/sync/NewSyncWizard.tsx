import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowLeft, ArrowLeftRight, ArrowRight, Check, File, Folder, FolderOpen, Monitor, Server } from "lucide-react";
import { Banner, Button, Checkbox, Dialog, Segmented, SelectField, TextField } from "../../components";
import { formatError } from "../../errors";
import { formatBytes, formatCount } from "./format";
import { directionOf, modesOf, type Direction } from "./direction";
import type { PathIssue, SyncEstimateView } from "./types";
import { pickFolder } from "../shell/pickFolder";
import "./sync.css";

export interface SyncDraft {
  serverId: string;
  remotePath: string;
  localPath: string;
  /** 服务器一侧的方式（receiveOnly = 服务器只接收本机的变化） */
  mode: "sendReceive" | "receiveOnly";
  /** 本机一侧的方式（receiveOnly = 本机只接收服务器的变化）；没写时为收发 */
  localMode?: "sendReceive" | "receiveOnly";
  name: string;
  rules: string[];
  collaboration: boolean;
  conflictMode: "ask" | "newest";
  /** 加入已有同步且两边排除规则不一致时，用户选择以哪份为准 */
  ignoresSource?: "server" | "local" | null;
}

interface Props {
  servers: { id: string; name: string; deployed: boolean }[];
  /** 新任务默认排除规则（来自设置） */
  defaultRules: string[];
  /** 新任务默认冲突处理（来自设置） */
  defaultConflictMode?: SyncDraft["conflictMode"];
  /** 检查目录（危险目录、重叠、网盘、已被别的 Syncthing 管理、可加入已有同步） */
  checkPaths: (d: SyncDraft) => Promise<PathIssue[]>;
  /** 开始前预估（Syncthing 扫描两端，按规则过滤后比较） */
  estimate: (d: SyncDraft, onProgress?: (local: number, remote: number) => void) => Promise<SyncEstimateView>;
  /** 取消进行中的预估（关闭向导或回到上一步时） */
  onCancelEstimate?: () => void;
  /** 创建任务；返回 Promise 时向导显示进行中，失败时在向导里显示原因 */
  onCreate: (d: SyncDraft, start: boolean) => void | Promise<void>;
  onClose: () => void;
  /** 服务器还没部署同步组件时，用户点“去部署” */
  onDeploy?: (serverId: string) => void;
  /** 预填内容（如从服务器文件浏览器“为此目录设置同步”进入时带上远程路径） */
  initial?: Partial<SyncDraft>;
  /** 开发预览用：直接从第几步开始 */
  initialStep?: 0 | 1 | 2;
  /** 给已有任务添加服务器：本机目录、名称、排除规则沿用任务的，跳过排除规则这一步 */
  addTo?: { name: string; localPath: string; localMode: "sendReceive" | "receiveOnly"; rules: string[] };
}

/** 会阻止继续的问题 */
const blocking = (i: PathIssue) => i.kind === "dangerous" || i.kind === "overlap" || i.kind === "otherSyncthing" || i.kind === "unreadable";
/** 添加服务器时：服务器上这个目录已经在被同步，不能作为另一台加进来 */
const blockingAdd = (i: PathIssue) => blocking(i) || i.kind === "joinExisting";
/** 添加服务器时只看服务器这一侧的问题（本机目录在建任务时已经检查过） */
const remoteOnly = (i: PathIssue) => i.kind !== "cloudFolder" && !("side" in i && i.side === "local");

/** 从路径取最后一段作为默认任务名 */
function lastSegment(p: string): string {
  return p.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? "";
}

function Stepper({ step, add }: { step: number; add?: boolean }) {
  const { t } = useTranslation();
  // 添加服务器没有“排除规则”这一步
  const names = add ? [t("newSync.step.server"), t("newSync.step.preview")] : [t("newSync.step.paths"), t("newSync.step.rules"), t("newSync.step.preview")];
  if (add) step = step === 2 ? 1 : 0;
  return (
    <ol className="wiz-steps">
      {names.map((n, i) => (
        <li key={n} aria-current={i === step ? "step" : undefined} className={i < step ? "is-done" : i === step ? "is-current" : ""}>
          <span className="wiz-steps__mark">{i < step ? <Check size={11} strokeWidth={3} aria-hidden /> : i + 1}</span>
          {n}
        </li>
      ))}
    </ol>
  );
}

function IssueBanner({ issue }: { issue: PathIssue }) {
  const { t } = useTranslation();
  const side = "side" in issue ? t(`newSync.side.${issue.side}`) : "";
  switch (issue.kind) {
    case "dangerous": return <Banner tone="danger" title={t("newSync.issue.dangerousTitle", { side })}>{t("newSync.issue.dangerousBody", { path: issue.path })}</Banner>;
    case "overlap": return <Banner tone="danger" title={t("newSync.issue.overlapTitle", { side })}>{t("newSync.issue.overlapBody", { task: issue.task })}</Banner>;
    case "cloudFolder": return <Banner tone="warn" title={t("newSync.issue.cloudTitle", { provider: issue.provider })}>{t("newSync.issue.cloudBody")}</Banner>;
    case "otherSyncthing": return <Banner tone="danger" title={t("newSync.issue.otherTitle", { side })}>{t("newSync.issue.otherBody")}</Banner>;
    case "joinExisting": return <Banner tone="info" title={t("newSync.issue.joinTitle")}>{t("newSync.issue.joinBody", { computer: issue.computer })}</Banner>;
    case "ignoreFile": return <Banner tone="info" title={t("newSync.issue.ignoreFileTitle", { side })}>{t("newSync.issue.ignoreFileBody")}</Banner>;
    case "gitRepository": return <Banner tone="info" title={t("newSync.issue.gitTitle", { side })}>{t("newSync.issue.gitBody")}</Banner>;
    case "unreadable": return <Banner tone="danger" title={t("newSync.issue.unreadableTitle", { side })}>{t("newSync.issue.unreadableBody")}</Banner>;
  }
}

/** 新建同步任务：选择目录 → 排除规则 → 预览与确认。创建后默认先暂停，由用户决定何时开始。 */
export function NewSyncWizard({ servers, defaultRules, defaultConflictMode = "ask", checkPaths, estimate, onCancelEstimate, onCreate, onClose: close, onDeploy, initial, initialStep = 0, addTo }: Props) {
  const { t, i18n } = useTranslation();
  const n = (v: number) => formatCount(v, i18n.language);
  const [step, setStep] = useState<number>(initialStep);
  const [draft, setDraft] = useState<SyncDraft>({
    serverId: servers[0]?.id ?? "", remotePath: "", localPath: "", mode: "sendReceive", name: "",
    rules: defaultRules, collaboration: false, conflictMode: defaultConflictMode, ...initial,
    ...(addTo ? {
      localPath: addTo.localPath, name: addTo.name, rules: addTo.rules, localMode: addTo.localMode,
      // 本机只接收时服务器必须收发；否则默认双向
      mode: "sendReceive" as const,
    } : {}),
  });
  const [nameTouched, setNameTouched] = useState(false);
  const dir = directionOf(draft.localMode ?? "sendReceive", [draft.mode]);
  const direction: Direction = dir === "mixed" ? "both" : dir;
  const [issues, setIssues] = useState<PathIssue[] | null>(null);
  const [checking, setChecking] = useState(false);
  const [rulesText, setRulesText] = useState(defaultRules.join("\n"));
  const [est, setEst] = useState<SyncEstimateView | null>(null);
  const [estError, setEstError] = useState<string | null>(null);
  /** 后端暂不支持开始前预估 */
  const [estUnavailable, setEstUnavailable] = useState(false);
  const [creating, setCreating] = useState(false);
  const [scanned, setScanned] = useState<{ local: number; remote: number } | null>(null);
  const [createError, setCreateError] = useState<string | null>(null);
  /** 加入已有同步时两边规则不一致：服务器与本机各几条 */
  const [mismatch, setMismatch] = useState<{ server: string; local: string; start: boolean } | null>(null);
  const [ack, setAck] = useState(false);

  const set = (patch: Partial<SyncDraft>) => setDraft((d) => ({ ...d, ...patch }));
  const server = servers.find((s) => s.id === draft.serverId);
  const pathsFilled = draft.remotePath.trim() !== "" && draft.localPath.trim() !== "";
  const blocked = (issues ?? []).some(addTo ? blockingAdd : blocking);
  const rules = useMemo(() => rulesText.split(/\r?\n/).map((l) => l.trim()).filter(Boolean), [rulesText]);
  const gitRemoved = !rules.some((r) => /^(\(\?d\))?\.git\/?$/.test(r));

  // 路径变化后旧的检查结果作废
  useEffect(() => { setIssues(null); }, [draft.serverId, draft.remotePath, draft.localPath]);

  const runEstimate = (d: SyncDraft) => {
    setEst(null);
    setEstError(null);
    setEstUnavailable(false);
    setCreateError(null);
    setAck(false);
    setScanned(null);
    estimate(d, (local, remote) => setScanned({ local, remote })).then(setEst, (e: unknown) => {
      if ((e as { code?: string } | null)?.code === "sync.estimate_unavailable") setEstUnavailable(true);
      else setEstError(formatError(e));
    });
  };
  // 直接从第三步开始时（开发预览）也要做预估
  useEffect(() => {
    if (initialStep === 2) runEstimate(draft);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const next = async () => {
    if (step === 0) {
      setChecking(true);
      try {
        const all = await checkPaths(draft);
        const found = addTo ? all.filter(remoteOnly) : all;
        setIssues(found);
        if (addTo && !found.some(blockingAdd)) { setStep(2); runEstimate(draft); }
        else if (!addTo && !found.some(blocking)) setStep(1);
      } finally { setChecking(false); }
    } else if (step === 1) {
      const d = { ...draft, rules };
      setDraft(d);
      setStep(2);
      runEstimate(d);
    }
  };

  const canCreate = ack && !creating && (!!est?.done || estUnavailable);
  const estimating = step === 2 && !est && !estError && !estUnavailable;
  // 预估进行中关闭向导或回到上一步时取消预估
  const onClose = () => { if (estimating) onCancelEstimate?.(); close(); };
  const create = async (start: boolean, ignoresSource: SyncDraft["ignoresSource"] = null) => {
    setCreating(true);
    setCreateError(null);
    setMismatch(null);
    try {
      await onCreate({ ...draft, ignoresSource }, start);
    } catch (e) {
      const err = e as { code?: string; params?: Record<string, string> } | null;
      if (err?.code === "sync.ignores_mismatch") {
        setMismatch({ server: err.params?.server_count ?? "0", local: err.params?.local_count ?? String(draft.rules.length), start });
      } else {
        setCreateError(formatError(e));
      }
    } finally {
      setCreating(false);
    }
  };

  const footer = (
    <>
      {step > 0 && <Button onClick={() => { if (estimating) onCancelEstimate?.(); setStep(addTo ? 0 : step - 1); }}>{t("newSync.back")}</Button>}
      <span style={{ flex: 1 }} />
      <Button variant="ghost" onClick={onClose}>{t("common.cancel")}</Button>
      {step < 2 && (
        <Button variant="primary" disabled={(step === 0 && (!pathsFilled || checking || blocked || !draft.name.trim() || !server?.deployed)) || (step === 1 && rules.length > 10000)} onClick={() => void next()}>
          {checking ? t("newSync.checking") : t("newSync.next")}
        </Button>
      )}
      {step === 2 && <>
        {/* 添加服务器只有一个按钮：任务在同步就一起同步，任务暂停着就一起暂停（不会出现单独停着的一台） */}
        {!addTo && <Button disabled={!canCreate} onClick={() => void create(false)}>{t("newSync.createPaused")}</Button>}
        <Button variant="primary" disabled={!canCreate} onClick={() => void create(true)}>{creating ? t("newSync.creating") : addTo ? t("newSync.addStart") : t("newSync.createStart")}</Button>
      </>}
    </>
  );

  return (
    <Dialog title={addTo ? t("newSync.addTitle", { name: addTo.name }) : t("newSync.title")} size="xl" onDismiss={onClose} actions={footer}>
      <div className="wiz">
        <Stepper step={step} add={!!addTo} />

        {step === 0 && (
          <div className="wiz__body">
            <div className="wiz-grid">
              <SelectField
                label={t("newSync.server")}
                value={draft.serverId}
                onChange={(v) => set({ serverId: v })}
                options={servers.map((s) => ({ value: s.id, label: s.name }))}
              />
              <div className="wiz-field">
                <span className="wiz-field__label">{t("newSync.mode")}</span>
                <Segmented<Direction>
                  label={t("newSync.mode")}
                  value={direction}
                  onChange={(d) => { const m = modesOf(d); set(addTo ? { mode: m.serverMode } : { mode: m.serverMode, localMode: m.localMode }); }}
                  // 添加服务器时本机一侧的方式由任务定：本机只接收时只能“服务器 → 本机”，否则可以双向或只发到服务器
                  options={(addTo ? (addTo.localMode === "receiveOnly" ? ["toLocal"] as const : ["both", "toServer"] as const) : ["both", "toServer", "toLocal"] as const).map((d) => ({ value: d, label: t(`sync.direction.${d}`) }))}
                />
              </div>
            </div>
            {server && !server.deployed && (
              <Banner
                tone="warn"
                title={t("newSync.notDeployedTitle")}
                actions={onDeploy && <Button size="sm" variant="primary" onClick={() => onDeploy(server.id)}>{t("newSync.goDeploy")}</Button>}
              >
                {t("newSync.notDeployedBody")}
              </Banner>
            )}
            <TextField
              label={t("newSync.remotePath")}
              className="pl-input mono"
              placeholder={t("newSync.remotePathPlaceholder")}
              value={draft.remotePath}
              onChange={(e) => {
                const remotePath = e.target.value;
                set({ remotePath, ...(nameTouched || addTo ? {} : { name: lastSegment(remotePath) }) });
              }}
            />
            {addTo ? (
              <p className="wiz-hint">{t("newSync.addLocal", { path: addTo.localPath })}</p>
            ) : <>
            <div className="wiz-pick">
              <TextField
                label={t("newSync.localPath")}
                className="pl-input mono"
                placeholder={t("newSync.localPathPlaceholder")}
                value={draft.localPath}
                onChange={(e) => set({ localPath: e.target.value })}
              />
              <Button icon={<FolderOpen size={13} />} onClick={() => void pickFolder().then((p) => { if (p) set({ localPath: p }); }, () => {})}>{t("mcp.pickFolder")}</Button>
            </div>
            <TextField label={t("newSync.name")} value={draft.name} onChange={(e) => { setNameTouched(true); set({ name: e.target.value }); }} />
            </>}
            <p className="wiz-hint">{t(`newSync.modeHint.${direction}`)}</p>
            {issues?.map((i, k) => <IssueBanner key={k} issue={i} />)}
          </div>
        )}

        {step === 1 && (
          <div className="wiz__body">
            <p className="wiz-hint">{t("newSync.rulesIntro")}</p>
            <label className="wiz-field">
              <span className="wiz-field__label">{t("newSync.rulesLabel", { count: rules.length })}</span>
              <textarea className="pl-input mono wiz-rules" spellCheck={false} value={rulesText} onChange={(e) => setRulesText(e.target.value)} rows={12} />
            </label>
            <div className="wiz-rules__actions">
              <Button size="sm" variant="ghost" onClick={() => setRulesText(defaultRules.join("\n"))}>{t("newSync.rulesReset")}</Button>
            </div>
            <ul className="wiz-notes">
              <li>{t("newSync.rulesNoteSyntax")}</li>
              <li>{t("newSync.rulesNoteD")}</li>
              <li>{t("newSync.rulesNoteSensitive")}</li>
            </ul>
            {gitRemoved && <Banner tone="warn" title={t("newSync.gitRemovedTitle")}>{t("newSync.gitRemovedBody")}</Banner>}
          </div>
        )}

        {step === 2 && (
          <div className="wiz__body">
            <div className="wiz-map">
              <div className="wiz-map__side">
                <span className="wiz-map__head"><Server size={15} aria-hidden />{t("newSync.serverSide", { name: server?.name ?? "" })}</span>
                <span className="mono wiz-map__path">{draft.remotePath}</span>
                {est && <span className="wiz-map__meta">{t("newSync.total", { count: est.remoteTotal.files, n: n(est.remoteTotal.files), size: formatBytes(est.remoteTotal.bytes) })}</span>}
                {est && <span className="wiz-map__meta wiz-map__meta--dim">{t("newSync.afterRules", { count: est.remoteIncluded.files, n: n(est.remoteIncluded.files), size: formatBytes(est.remoteIncluded.bytes) })}</span>}
              </div>
              <span className="wiz-map__arrow">{direction === "both" ? <ArrowLeftRight size={22} aria-hidden /> : direction === "toLocal" ? <ArrowRight size={22} aria-hidden /> : <ArrowLeft size={22} aria-hidden />}{t(`sync.direction.${direction}`)}</span>
              <div className="wiz-map__side">
                <span className="wiz-map__head"><Monitor size={15} aria-hidden />{t("newSync.localSide")}</span>
                <span className="mono wiz-map__path">{draft.localPath}</span>
                {est && <span className="wiz-map__meta">{est.localTotal.files === 0 ? t("newSync.localEmpty") : t("newSync.total", { count: est.localTotal.files, n: n(est.localTotal.files), size: formatBytes(est.localTotal.bytes) })}</span>}
                {est && est.localTotal.files > 0 && <span className="wiz-map__meta wiz-map__meta--dim">{t("newSync.afterRules", { count: est.localIncluded.files, n: n(est.localIncluded.files), size: formatBytes(est.localIncluded.bytes) })}</span>}
              </div>
            </div>

            {!addTo && <div className="wiz-options">
              <Checkbox label={t("newSync.collaboration")} checked={draft.collaboration} onChange={(e) => set({ collaboration: e.target.checked })} />
              <span style={{ flex: 1 }} />
              <SelectField
                label={t("sync.conflictMode")}
                value={draft.conflictMode}
                onChange={(v) => set({ conflictMode: v as SyncDraft["conflictMode"] })}
                options={[{ value: "ask", label: t("sync.conflictModes.ask") }, { value: "newest", label: t("sync.conflictModes.newest") }]}
              />
            </div>}

            {estError && <Banner tone="danger" title={t("newSync.estimateFailed")}>{estError}</Banner>}
            {estUnavailable && <Banner tone="info" title={t("newSync.estimateLaterTitle")}>{t("newSync.estimateLaterBody")}</Banner>}
            {estimating && (
              <div className="wiz-scan" role="status">
                {t("newSync.scanning")}
                {scanned && <div className="sync-muted">{t("newSync.scanningProgress", { local: scanned.local, remote: scanned.remote })}</div>}
              </div>
            )}
            {est && (
              <>
                {est.localTotal.files === 0
                  ? <Banner tone="info" title={t("newSync.firstDownTitle")}>{t("newSync.firstDownBody", { n: n(est.download.files), size: formatBytes(est.download.bytes) })}</Banner>
                  : est.remoteTotal.files === 0
                    ? <Banner tone="info" title={t("newSync.firstUpTitle")}>{t("newSync.firstUpBody", { n: n(est.upload.files), size: formatBytes(est.upload.bytes) })}</Banner>
                    : <Banner tone="warn" title={t("newSync.mergeTitle")}>{t("newSync.mergeBody", { count: est.conflictCount })}</Banner>}

                <div className="wiz-section">
                  <span className="ws__label">{t("newSync.estimate")}</span>
                  <span className="sync-muted">{t("newSync.estimateNote", { remote: n(est.scannedRemote), local: n(est.scannedLocal) })}</span>
                </div>
                <div className="wiz-tiles">
                  <div className="wiz-tile wiz-tile--up"><span>{t("newSync.tile.upload")}</span><b>{n(est.upload.files)}</b><small>{formatBytes(est.upload.bytes)}</small></div>
                  <div className="wiz-tile wiz-tile--down"><span>{t("newSync.tile.download")}</span><b>{n(est.download.files)}</b><small>{formatBytes(est.download.bytes)}</small></div>
                  <div className="wiz-tile"><span>{t("newSync.tile.same")}</span><b>{n(est.same)}</b><small>{t("newSync.tile.sameSub")}</small></div>
                  <div className={`wiz-tile${est.conflictCount > 0 ? " wiz-tile--conflict" : ""}`}><span>{t("newSync.tile.conflict")}</span><b>{n(est.conflictCount)}</b><small>{t("newSync.tile.conflictSub")}</small></div>
                </div>
                {est.conflicts.length > 0 && (
                  <div className="wiz-list wiz-list--warn">
                    {est.conflicts.map((c) => (
                      <div key={c.path} className="wiz-list__row">
                        <span className="mono">{c.path}</span>
                        <span className="sync-muted">{t(c.newer === "local" ? "newSync.newerLocal" : "newSync.newerRemote", { local: c.localTime, remote: c.remoteTime })}</span>
                      </div>
                    ))}
                    {est.conflictCount > est.conflicts.length && (
                      <button type="button" className="wiz-list__more">{t("newSync.allConflicts", { count: est.conflictCount })}</button>
                    )}
                  </div>
                )}

                <div className="wiz-two">
                  <div className="wiz-section-col">
                    <span className="ws__label">{t("newSync.willSync")}</span>
                    <div className="wiz-list">
                      {est.included.map((f) => (
                        <div key={f.path} className="wiz-list__row">
                          {f.path.endsWith("/") ? <Folder size={13} aria-hidden /> : <File size={13} aria-hidden />}
                          <span className="mono">{f.path}</span>
                          <span className="sync-muted">{f.path.endsWith("/") ? t("newSync.filesSize", { count: f.files, n: n(f.files), size: formatBytes(f.bytes) }) : formatBytes(f.bytes)}</span>
                        </div>
                      ))}
                    </div>
                  </div>
                  <div className="wiz-section-col">
                    <span className="ws__label">{t("newSync.excluded")}</span>
                    <div className="wiz-list">
                      {est.excluded.map((f) => (
                        <div key={f.path} className="wiz-list__row wiz-list__row--excluded"><span className="mono">{f.path}</span><span className="sync-muted">{t(`newSync.reason.${f.reason}`)}{f.bytes !== null ? ` · ${formatBytes(f.bytes)}` : ""}</span></div>
                      ))}
                    </div>
                  </div>
                </div>
              </>
            )}

            <div className="wiz-risk">
              <b>{t("newSync.riskTitle")}</b>
              <ul>
                <li>{t("newSync.risk1")}</li>
                <li>{t("newSync.risk2")}</li>
                <li>{t("newSync.risk3")}</li>
              </ul>
              <Checkbox label={t("newSync.ack")} checked={ack} onChange={(e) => setAck(e.target.checked)} />
            </div>
            {createError && <Banner tone="danger" title={t("newSync.createFailed")}>{createError}</Banner>}
            {mismatch && (
              <Banner
                tone="warn"
                title={t("newSync.mismatchTitle")}
                actions={<>
                  <Button size="sm" onClick={() => void create(mismatch.start, "server")}>{t("newSync.mismatchUseServer", { count: Number(mismatch.server) })}</Button>
                  <Button size="sm" variant="primary" onClick={() => void create(mismatch.start, "local")}>{t("newSync.mismatchUseLocal", { count: Number(mismatch.local) })}</Button>
                </>}
              >
                {t("newSync.mismatchBody", { server: mismatch.server, local: mismatch.local })}
              </Banner>
            )}
          </div>
        )}
      </div>
    </Dialog>
  );
}
