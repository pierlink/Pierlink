import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Monitor, Plus, Server, Trash2 } from "lucide-react";
import { Banner, Button, Checkbox, Dialog, Notice, Segmented, Select, Switch } from "../../components";
import { formatError } from "../../errors";
import { getServersApi, type ServerRecord } from "../servers/api";
import { defaultServerEntry, getMcpApi, type EntrySettings, type McpActivity, type McpConnectInfo, type McpEntryState, type McpJob, type McpSettings, type McpStatus } from "./api";
import { CommandLine, EntryPermissions, PortInput } from "./McpParts";
import "./mcp.css";

type Tab = "entries" | "activity";
/** 选中的入口：本机（windows）或服务器 ID */
type EntryId = string;

/**
 * 顶部导航里的“MCP”板块：让 Claude Code 等 AI 助手通过 Pierlink 操作这台电脑和服务器。
 * 两个标签：入口（左边入口列表，右边选中入口的接入命令、审批方式、权限、端口和令牌）、操作记录（全部 AI 操作）。
 * 改动立即保存，按顺序一个接一个保存。
 */
/**
 * 打开 AI 接入的总开关。一个入口都还没开时顺带打开本机入口：
 * 否则一键添加到客户端以后，AI 一连上就被拒，用户不知道还要再开一个开关。
 */
function turnOn(settings: McpSettings): McpSettings {
  const anyEntry = settings.windows.enabled || settings.servers.some((s) => s.entry.enabled);
  return { ...settings, enabled: true, windows: anyEntry ? settings.windows : { ...settings.windows, enabled: true } };
}

export function McpPage() {
  const { t } = useTranslation();
  const [settings, setSettings] = useState<McpSettings | null>(null);
  const [status, setStatus] = useState<McpStatus | null>(null);
  const [info, setInfo] = useState<McpConnectInfo | null>(null);
  const [servers, setServers] = useState<ServerRecord[]>([]);
  const [activity, setActivity] = useState<McpActivity[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>("entries");
  const [selected, setSelected] = useState<EntryId>("windows");
  const [adding, setAdding] = useState<string[] | null>(null);
  const [stopping, setStopping] = useState(false);
  const saving = useRef(Promise.resolve());

  useEffect(() => {
    let alive = true;
    const offs: (() => void)[] = [];
    const keep = (u: () => void) => { if (alive) offs.push(u); else u(); };
    void getMcpApi().then(async (api) => {
      try {
        const [s, st, i, a] = await Promise.all([api.getSettings(), api.status(), api.connectInfo(), api.activity(200)]);
        if (!alive) return;
        setSettings(s); setStatus(st); setInfo(i); setActivity(a);
      } catch (e) {
        if (alive) setError(formatError(e));
      }
      void api.onEntries((entries) => setStatus((st) => (st ? { ...st, entries } : st))).then(keep);
      void api.onSessions(() => void api.status().then((st) => { if (alive) setStatus(st); }, () => {})).then(keep);
      void api.onActivity((a) => setActivity((list) => [a, ...list].slice(0, 500))).then(keep);
    });
    void getServersApi().then((api) => api.list()).then((list) => { if (alive) setServers(list); }, () => {});
    return () => { alive = false; offs.forEach((u) => u()); };
  }, []);

  /** 保存（按顺序，后一次等前一次）；失败时恢复成后端的设置并显示原因 */
  const save = (next: McpSettings) => {
    setSettings(next);
    setError(null);
    saving.current = saving.current.then(async () => {
      const api = await getMcpApi();
      try {
        setSettings(await api.setSettings(next));
        setInfo(await api.connectInfo());
      } catch (e) {
        setError(formatError(e));
        setSettings(await api.getSettings());
      }
    });
  };

  const serverName = (id: string) => (id === "windows" ? t("mcp.entryWindows") : servers.find((s) => s.id === id)?.name ?? id);
  const sessions = status?.sessions ?? [];
  const sessionsOf = (id: EntryId) => sessions.filter((s) => s.entry === id);
  const entryState = (id: string): McpEntryState | undefined => status?.entries.find((x) => x.serverId === id);

  const stopAll = async () => {
    setStopping(true);
    try { await (await getMcpApi()).stopAll(); } catch (e) { setError(formatError(e)); } finally { setStopping(false); }
  };

  if (!settings) {
    return <section className="pl-panel mcp-page__empty">{error ? <Banner tone="danger" title={error} /> : null}</section>;
  }

  return (
    <div className="mcp-page">
      <header className="pl-panel mcp-page__head">
        <div className="mcp-page__title">
          <h1>{t("nav.mcp")}</h1>
          <Switch label={t("mcp.enable")} checked={settings.enabled} onChange={(v) => save(v ? turnOn(settings) : { ...settings, enabled: false })} />
        </div>
        <Segmented<Tab> label={t("nav.mcp")} value={tab} onChange={setTab} options={[
          { value: "entries", label: t("mcp.tabEntries") },
          { value: "activity", label: t("mcp.tabActivity") },
        ]} />
        <span className="mcp-page__spacer" />
        {/* 连着的 AI 数和“全部停止”占固定位置：数量变化、按钮出现消失都不挤动别的内容 */}
        <span className="mcp-page__sessions">{t("mcp.connectedCount", { count: sessions.length })}</span>
        {/* 没有连着的 AI 也能点：后台任务、记住的拒绝都靠它清掉 */}
        <Button size="sm" variant="danger-ghost" disabled={stopping} title={t("mcp.stopAllHint")} onClick={() => void stopAll()}>{t("mcp.stopAll")}</Button>
      </header>

      {/* 出错提示浮在底部提示区，不把列表和详情往下推；点“知道了”或下一次操作时消失 */}
      {error && (
        <Notice order={0}>
          <div className="ws-load-error">
            <Banner tone="danger" title={error} actions={<Button size="sm" variant="ghost" onClick={() => setError(null)}>{t("mcp.gotIt")}</Button>} />
          </div>
        </Notice>
      )}

      {!settings.enabled ? (
        <section className="pl-panel mcp-page__off">
          <p>{t("mcp.intro")}</p>
          <p className="set-row__hint">{t("mcp.enableHint")}</p>
          <Button variant="primary" onClick={() => save(turnOn(settings))}>{t("mcp.enable")}</Button>
        </section>
      ) : tab === "entries" ? (
        <div className="mcp-page__body">
          <nav className="pl-panel mcp-list" aria-label={t("mcp.tabEntries")}>
            <EntryRow
              icon={<Monitor size={15} />}
              name={t("mcp.entryWindows")}
              sub={t("mcp.entryWindowsSub")}
              on={settings.windows.enabled}
              state={settings.windows.enabled ? (status && !status.pipeRunning ? "error" : "running") : "stopped"}
              sessions={sessionsOf("windows").length}
              selected={selected === "windows"}
              onSelect={() => setSelected("windows")}
            />
            <div className="mcp-list__label">{t("mcp.serverEntries")}</div>
            {settings.servers.length === 0 && <p className="sync-muted mcp-list__empty">{t("mcp.noServerEntries")}</p>}
            {settings.servers.map((s) => (
              <EntryRow
                key={s.serverId}
                icon={<Server size={15} />}
                name={serverName(s.serverId)}
                sub={`127.0.0.1:${s.port}`}
                on={s.entry.enabled}
                state={s.entry.enabled ? entryState(s.serverId)?.state ?? "connecting" : "stopped"}
                sessions={sessionsOf(s.serverId).length}
                selected={selected === s.serverId}
                onSelect={() => setSelected(s.serverId)}
              />
            ))}
            <button type="button" className="sync-map__add mcp-list__add" onClick={() => setAdding([])}>
              <Plus size={13} />{t("mcp.addEntries")}
            </button>
          </nav>
          <EntryDetail
            id={selected}
            settings={settings}
            servers={servers}
            status={status}
            info={info}
            serverName={serverName}
            sessions={sessionsOf(selected)}
            onSave={save}
            onRemoved={() => setSelected("windows")}
          />
        </div>
      ) : (
        <ActivityView activity={activity} settings={settings} serverName={serverName} onError={setError} />
      )}

      {adding && (
        <AddEntriesDialog
          servers={servers.filter((s) => !settings.servers.some((x) => x.serverId === s.id))}
          picked={adding}
          onPick={setAdding}
          onCancel={() => setAdding(null)}
          onAdd={() => {
            save({ ...settings, servers: [...settings.servers, ...adding.map(defaultServerEntry)] });
            if (adding.length) setSelected(adding[0]);
            setAdding(null);
          }}
        />
      )}
    </div>
  );
}

/** 左边列表里的一个入口 */
function EntryRow({ icon, name, sub, on, state, sessions, selected, onSelect }: {
  icon: React.ReactNode; name: string; sub: string; on: boolean; state: string; sessions: number; selected: boolean; onSelect: () => void;
}) {
  const { t } = useTranslation();
  return (
    <button type="button" className="task-row mcp-row" aria-current={selected} onClick={onSelect}>
      <span className="task-row__head">
        <span className={`mcp-row__dot mcp-row__dot--${on ? state : "stopped"}`} aria-hidden="true" />
        <span className="mcp-row__icon" aria-hidden="true">{icon}</span>
        <span className="task-row__name pl-ellipsis">{name}</span>
        <span className="task-row__spacer" />
        {/* 连着的 AI：没有时也占位，数量出现不改变行宽 */}
        <span className={`mcp-row__count${sessions ? "" : " mcp-row__count--none"}`}>{sessions ? t("mcp.aiCount", { count: sessions }) : "·"}</span>
      </span>
      <span className="task-row__detail mono">{on ? t(`mcp.entryState.${state}`, { defaultValue: state }) : t("mcp.entryOff")} · {sub}</span>
    </button>
  );
}

/** 右边：选中入口的详情 */
function EntryDetail({ id, settings, servers, status, info, serverName, sessions, onSave, onRemoved }: {
  id: EntryId;
  settings: McpSettings;
  servers: ServerRecord[];
  status: McpStatus | null;
  info: McpConnectInfo | null;
  serverName: (id: string) => string;
  sessions: McpStatus["sessions"];
  onSave: (s: McpSettings) => void;
  onRemoved: () => void;
}) {
  const { t } = useTranslation();
  const windows = id === "windows";
  const index = settings.servers.findIndex((s) => s.serverId === id);
  const server = index >= 0 ? settings.servers[index] : null;
  if (!windows && !server) return <section className="pl-panel sync-detail" />;
  const entry: EntrySettings = windows ? settings.windows : server!.entry;
  const setEntry = (next: EntrySettings) => onSave(windows
    ? { ...settings, windows: next }
    : { ...settings, servers: settings.servers.map((s, j) => (j === index ? { ...s, entry: next } : s)) });
  const st = server ? status?.entries.find((x) => x.serverId === id) : undefined;
  const commands = windows ? info?.windows ?? [] : info?.servers.find((x) => x.serverId === id)?.commands ?? [];
  const [rotated, setRotated] = useState<{ ok: boolean; text: string } | null>(null);
  useEffect(() => { setRotated(null); }, [id]);
  const rotate = async () => {
    setRotated(null);
    try {
      await (await getMcpApi()).rotateToken(id);
      setRotated({ ok: true, text: t("mcp.rotated") });
    } catch (e) {
      setRotated({ ok: false, text: formatError(e) });
    }
  };

  return (
    <section className="pl-panel sync-detail mcp-detail">
      <div className="sync-detail__head">
        <h2>{serverName(id)}</h2>
        <span className="sync-detail__spacer" />
        <Switch label={windows ? t("mcp.windowsEntry") : t("mcp.serverEntryEnable")} checked={entry.enabled} onChange={(v) => setEntry({ ...entry, enabled: v })} />
        {server && (
          <Button size="sm" variant="danger-ghost" icon={<Trash2 size={13} />}
            onClick={() => { onSave({ ...settings, servers: settings.servers.filter((_, j) => j !== index) }); onRemoved(); }}>{t("mcp.removeEntry")}</Button>
        )}
      </div>
      <p className="set-row__hint">{windows ? t("mcp.windowsEntryHint") : t("mcp.serverEntriesHint")}</p>
      {windows && status && !status.pipeRunning && <Banner tone="warn" title={t("mcp.pipeUnavailable")}>{status.pipeError}</Banner>}
      {st?.error && <Banner tone="danger" title={formatError(st.error)} />}
      {server && <Banner tone="warn" title={t("mcp.serverRiskShort")} />}

      <div className="mcp-detail__grid">
        <div className="mcp-detail__card">
          <h3>{t("mcp.clientsTitle")}</h3>
          {/* 服务器上的手动命令从 ~/.pierlink/mcp.json 取令牌：入口连上之前这个文件还没写 */}
          {/* 换了入口就重建：上一个入口还在添加、移除时，结果不会显示到新入口上 */}
          <ClientSetup key={id} serverId={windows ? null : id} serverName={serverName(id)} commands={commands}
            fileHint={server && st ? (st.fileWritten ? t("mcp.fileWritten") : t("mcp.fileNotWritten")) : undefined} />
          {server && (
            <div className="mcp-detail__row">
              <PortInput label={t("mcp.port")} value={server.port} onCommit={(port) => onSave({ ...settings, servers: settings.servers.map((s, j) => (j === index ? { ...s, port } : s)) })} />
              <Button size="sm" title={t("mcp.rotateTokenHint")} onClick={() => void rotate()}>{t("mcp.rotateToken")}</Button>
            </div>
          )}
          {server && (
            <span className={`mcp-oneclick__result pl-ellipsis${rotated ? (rotated.ok ? " mcp-oneclick__result--ok" : " mcp-oneclick__result--error") : ""}`} role="status" title={rotated?.text}>{rotated?.text ?? ""}</span>
          )}
          <h3>{t("mcp.sessionsOfEntry")}</h3>
          {sessions.length === 0
            ? <p className="sync-muted">{t("mcp.noSessions")}</p>
            : <ul className="mcp-detail__sessions">{sessions.map((s) => <li key={s.id} className="pl-ellipsis">{s.client || t("mcp.unknownClient")}</li>)}</ul>}
          {entry.remembered && (
            <div className="mcp-remembered">{t("mcp.remembered")}<Button size="sm" variant="ghost" onClick={() => setEntry({ ...entry, remembered: false })}>{t("mcp.forget")}</Button></div>
          )}
        </div>
        <div className="mcp-detail__card">
          <h3>{t("mcp.sectionPermissions")}</h3>
          <EntryPermissions entry={entry} servers={servers} exclude={server?.serverId} windowsWarning={!windows} onChange={setEntry} />
        </div>
      </div>
    </section>
  );
}

/** 客户端的显示名 */
const CLIENT_NAMES = { claude: "Claude Code", codex: "Codex", cursor: "Cursor", gemini: "Gemini CLI" } as const;
type ClientKey = keyof typeof CLIENT_NAMES;
/** 本机能一键添加的；服务器上能一键添加的 */
const LOCAL_CLIENTS: readonly ClientKey[] = ["claude", "codex", "cursor"];
const SERVER_CLIENTS: readonly ClientKey[] = ["claude", "codex", "gemini"];

/**
 * 添加到 AI 客户端：上面一排标签选客户端（没装的也能选），下面是这个客户端的手动命令、“一键添加”和“移除”。
 * 点标签只切换，不会添加；没装的客户端两个按钮灰掉。结果写在按钮下面固定高度的一行里，不挤动下面的内容
 */
function ClientSetup({ serverId, serverName, commands, fileHint }: {
  serverId: string | null; serverName: string; commands: { client: string; text: string }[]; fileHint?: string;
}) {
  const { t } = useTranslation();
  const clients = serverId === null ? LOCAL_CLIENTS : SERVER_CLIENTS;
  const [client, setClient] = useState<ClientKey>(clients[0]);
  // 用户自己点过标签后，检测结果回来也不再自动换到装了的那个
  const picked = useRef(false);
  const [busy, setBusy] = useState<"add" | "remove" | null>(null);
  const [result, setResult] = useState<{ ok: boolean; text: string } | null>(null);
  // 装了哪些客户端：本机直接查，服务器要连过去查；没装的按钮灰掉并说明。查不到（服务器连不上）时不限制
  const [installed, setInstalled] = useState<Record<ClientKey, boolean> | null>(null);
  useEffect(() => {
    let alive = true;
    picked.current = false;
    setClient(clients[0]);
    setResult(null);
    setInstalled(null);
    void getMcpApi().then((api) => (serverId === null ? api.detectClients() : api.detectServerClients(serverId))).then(
      (c) => {
        if (!alive) return;
        setInstalled(c);
        // 默认选第一个装了的
        if (!picked.current) setClient(clients.find((x) => c[x]) ?? clients[0]);
      },
      () => { if (alive) setInstalled({ claude: true, codex: true, cursor: true, gemini: true }); },
    );
    return () => { alive = false; };
    // clients 只由 serverId 决定
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [serverId]);

  const name = CLIENT_NAMES[client];
  const missing = installed !== null && !installed[client];
  const shown = commands.filter((c) => c.client === (client === "cursor" ? "json" : client));
  const run = async (kind: "add" | "remove", f: () => Promise<string>) => {
    setBusy(kind);
    setResult(null);
    try {
      setResult({ ok: true, text: await f() });
    } catch (e) {
      setResult({ ok: false, text: formatError(e) });
    } finally {
      setBusy(null);
    }
  };
  const add = () => run("add", async () => {
    const api = await getMcpApi();
    if (serverId !== null) {
      await api.installServer(serverId, client as "claude" | "codex" | "gemini");
      return t("mcp.addedServer", { server: serverName, client: name });
    }
    const detail = await api.installLocal(client as "claude" | "codex" | "cursor");
    return client === "cursor" ? t("mcp.addedCursor", { path: detail }) : t("mcp.added", { client: name });
  });
  const remove = () => run("remove", async () => {
    const api = await getMcpApi();
    const removed = serverId !== null
      ? await api.removeServer(serverId, client as "claude" | "codex" | "gemini")
      : await api.removeLocal(client as "claude" | "codex" | "cursor");
    if (!removed) return t("mcp.notAdded", { client: name });
    return serverId !== null ? t("mcp.removedServer", { server: serverName, client: name }) : t("mcp.removed", { client: name });
  });
  const manual = serverId === null
    ? (client === "cursor" ? t("mcp.manualCursor") : t("mcp.manualLocal"))
    : shown.length > 1 ? t("mcp.manualServerJq") : t("mcp.manualServer");
  return (
    <div className="mcp-oneclick">
      <Segmented<ClientKey>
        label={t("mcp.clientsTitle")}
        value={client}
        options={clients.map((c) => ({ value: c, label: CLIENT_NAMES[c] }))}
        onChange={(c) => { if (busy) return; picked.current = true; setClient(c); setResult(null); }}
      />
      {missing && (
        <p className="set-row__hint">
          {serverId === null ? t("mcp.clientMissing", { client: name }) : t("mcp.serverClientMissing", { client: name, server: serverName })}
        </p>
      )}
      {shown.length > 0 && <p className="set-row__hint">{manual}</p>}
      {shown.map((c) => <CommandLine key={c.text} text={c.text} />)}
      {fileHint && shown.length > 0 && <p className="set-row__hint">{fileHint}</p>}
      <div className="mcp-oneclick__buttons">
        {/* 还在检测时也能点（服务器离线时检测要等很久）；没装的话后端报“没检测到”，照常提示 */}
        <Button size="sm" variant="primary" disabled={busy !== null || missing} onClick={() => void add()}>
          {busy === "add" ? t("mcp.adding") : t("mcp.addOneClick")}
        </Button>
        <Button size="sm" disabled={busy !== null || missing} onClick={() => void remove()}>
          {busy === "remove" ? t("mcp.removing") : t("mcp.remove")}
        </Button>
      </div>
      <span className={`mcp-oneclick__result pl-ellipsis${result ? (result.ok ? " mcp-oneclick__result--ok" : " mcp-oneclick__result--error") : ""}`} role="status" title={result?.text}>
        {result?.text ?? ""}
      </span>
    </div>
  );
}

/** 一次添加多台服务器的入口 */
function AddEntriesDialog({ servers, picked, onPick, onCancel, onAdd }: {
  servers: ServerRecord[]; picked: string[]; onPick: (ids: string[]) => void; onCancel: () => void; onAdd: () => void;
}) {
  const { t } = useTranslation();
  const toggle = (id: string) => onPick(picked.includes(id) ? picked.filter((x) => x !== id) : [...picked, id]);
  return (
    <Dialog
      title={t("mcp.addEntries")}
      onDismiss={onCancel}
      actions={<>
        <Button onClick={onCancel}>{t("common.cancel")}</Button>
        <Button variant="primary" disabled={picked.length === 0} onClick={onAdd}>{t("mcp.addEntriesCount", { count: picked.length })}</Button>
      </>}
    >
      <p className="set-row__hint">{t("mcp.addEntriesHint")}</p>
      {servers.length === 0
        ? <p className="sync-muted">{t("mcp.noAddable")}</p>
        : <div className="mcp-add">{servers.map((s) => <Checkbox key={s.id} label={s.host ? `${s.name}  ·  ${s.username ? `${s.username}@` : ""}${s.host}` : s.name} checked={picked.includes(s.id)} onChange={() => toggle(s.id)} />)}</div>}
    </Dialog>
  );
}

/** 操作记录表格的列：键和默认宽度（像素）；“内容”列占剩下的宽度 */
const COLUMNS = [
  { key: "time", width: 130 }, { key: "client", width: 150 }, { key: "entry", width: 130 }, { key: "tool", width: 120 },
  { key: "target", width: 110 }, { key: "summary", width: 0 }, { key: "result", width: 170 },
] as const;
const WIDTHS_KEY = "pierlink.mcp.activityColumns";

/** 读记住的列宽；读不到用默认值 */
function loadWidths(): Record<string, number> {
  try {
    const saved = JSON.parse(window.localStorage.getItem(WIDTHS_KEY) ?? "{}") as Record<string, number>;
    return Object.fromEntries(COLUMNS.map((c) => [c.key, typeof saved[c.key] === "number" ? saved[c.key] : c.width]));
  } catch {
    return Object.fromEntries(COLUMNS.map((c) => [c.key, c.width]));
  }
}

/** “操作记录”标签：全部 AI 操作，可按入口筛选 */
/**
 * AI 开的后台任务（如 npm run dev）：还在运行的可以停掉；刚结束的显示退出码。没有任务时不显示。
 * 任务开始、结束时后端发事件，这里重新拉
 */
function JobsList({ serverName, onError }: { serverName: (id: string) => string; onError: (e: string) => void }) {
  const { t } = useTranslation();
  const [jobs, setJobs] = useState<McpJob[]>([]);
  const [stopping, setStopping] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    let off: (() => void) | undefined;
    const load = () => void getMcpApi().then((api) => api.jobs()).then((l) => { if (alive) setJobs(l); }, () => {});
    load();
    void getMcpApi().then((api) => api.onJobs(load)).then((u) => { if (alive) off = u; else u(); });
    return () => { alive = false; off?.(); };
  }, []);
  if (jobs.length === 0) return null;
  const where = (id: string) => (id === "windows" ? t("mcp.targetWindows") : serverName(id));
  const stop = (id: string) => {
    setStopping(id);
    void getMcpApi().then((api) => api.stopJob(id)).then(
      () => setStopping(null),
      (e: unknown) => { setStopping(null); onError(formatError(e)); },
    );
  };
  return (
    <div className="mcp-jobs">
      <div className="mcp-jobs__title">{t("mcp.jobs.title", { count: jobs.filter((j) => j.running).length })}</div>
      {jobs.map((j) => (
        <div key={j.id} className="mcp-job">
          <span className={`mcp-job__dot${j.running ? " mcp-job__dot--on" : ""}`} aria-hidden="true" />
          <code className="mcp-job__cmd pl-ellipsis" title={j.command}>{j.command}</code>
          <span className="mcp-job__meta">
            {where(j.target)} · {j.running ? t("mcp.jobs.running", { min: Math.max(1, Math.round((Date.now() - j.startedMs) / 60000)) }) : t("mcp.jobs.exited", { code: j.exitCode ?? "-" })}
          </span>
          {j.running && <Button size="sm" variant="danger-ghost" disabled={stopping === j.id} onClick={() => stop(j.id)}>{t("mcp.jobs.stop")}</Button>}
        </div>
      ))}
    </div>
  );
}

function ActivityView({ activity, settings, serverName, onError }: {
  activity: McpActivity[]; settings: McpSettings; serverName: (id: string) => string; onError: (e: string) => void;
}) {
  const { t } = useTranslation();
  const [filter, setFilter] = useState("");
  const [widths, setWidths] = useState<Record<string, number>>(loadWidths);
  /** 拖动表头右边的竖条调列宽；松手时记住 */
  const startResize = (key: string, e: React.PointerEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const th = (e.currentTarget as HTMLElement).parentElement;
    const start = th?.getBoundingClientRect().width ?? widths[key];
    let latest = widths;
    const move = (ev: PointerEvent) => {
      latest = { ...latest, [key]: Math.max(60, Math.round(start + ev.clientX - startX)) };
      setWidths(latest);
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      try { window.localStorage.setItem(WIDTHS_KEY, JSON.stringify(latest)); } catch { /* 记不住也不影响使用 */ }
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };
  const rows = useMemo(() => activity.filter((a) => !filter || a.entry === filter), [activity, filter]);
  // 今天的只显示时间，更早的带上月日（整列宽度固定，不会被截断）
  const time = (ms: number) => {
    const d = new Date(ms);
    const clock = d.toLocaleTimeString([], { hour12: false });
    return d.toDateString() === new Date().toDateString() ? clock : `${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")} ${clock}`;
  };
  const outcome = (a: McpActivity) => (a.outcome === "ok" ? t("mcp.outcome.ok") : a.outcome === "denied" || a.outcome === "cancelled" ? t(`mcp.outcome.${a.outcome}`) : t("mcp.outcome.failed"));
  return (
    <section className="pl-panel mcp-activity">
      <JobsList serverName={serverName} onError={onError} />
      <div className="mcp-activity__bar">
        <Select label={t("mcp.filterEntry")} value={filter} onChange={setFilter} options={[
          { value: "", label: t("mcp.filterAll") },
          { value: "windows", label: t("mcp.entryWindows") },
          ...settings.servers.map((s) => ({ value: s.serverId, label: serverName(s.serverId) })),
        ]} />
        <span className="mcp-page__spacer" />
        <span className="sync-muted">{t("mcp.auditHint")}</span>
        <Button size="sm" onClick={() => void getMcpApi().then((api) => api.openLogFolder()).catch((e: unknown) => onError(formatError(e)))}>{t("mcp.openLogFolder")}</Button>
      </div>
      {rows.length === 0 ? <p className="sync-muted mcp-activity__empty">{t("mcp.noActivity")}</p> : (
        <div className="mcp-activity__scroll">
          <table className="mcp-table">
            <colgroup>{COLUMNS.map((c) => <col key={c.key} style={widths[c.key] ? { width: widths[c.key] } : undefined} />)}</colgroup>
            <thead>
              <tr>
                {COLUMNS.map((c) => (
                  <th key={c.key}>
                    {t(`mcp.col.${c.key}`)}
                    <span className="mcp-table__resize" role="separator" aria-orientation="vertical" title={t("mcp.resizeColumn")} onPointerDown={(e) => startResize(c.key, e)} />
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((a, i) => (
                <tr key={`${a.timeMs}-${i}`} className={a.outcome === "ok" ? "" : "mcp-table__bad"}>
                  <td className="mono" title={new Date(a.timeMs).toLocaleString()}>{time(a.timeMs)}</td>
                  <td title={a.client || t("mcp.unknownClient")}>{a.client || t("mcp.unknownClient")}</td>
                  <td title={serverName(a.entry)}>{serverName(a.entry)}</td>
                  <td className="mono" title={a.tool}>{a.tool}</td>
                  <td title={a.target ? serverName(a.target) : undefined}>{a.target ? serverName(a.target) : "—"}</td>
                  <td className="mono mcp-table__summary" title={a.summary}>{a.summary}</td>
                  <td title={[outcome(a), a.approval === "user" || a.approval === "session" || a.approval === "auto" ? t(`mcp.approvedByLong.${a.approval}`) : ""].filter(Boolean).join(" · ")}>
                    <span className={`mcp-table__result mcp-table__result--${a.outcome === "ok" ? "ok" : "bad"}`}>{outcome(a)}</span>
                    {(a.approval === "user" || a.approval === "session" || a.approval === "auto") && <span className="mcp-table__approval">{t(`mcp.approvedByLong.${a.approval}`)}</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
