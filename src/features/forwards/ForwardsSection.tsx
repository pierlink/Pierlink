import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ExternalLink, Plus } from "lucide-react";
import { Banner, Button, Dialog, IconButton, SelectField, Switch, TextField, useContextMenu } from "../../components";
import { formatError, isAppError } from "../../errors";
import { describeForward, getForwardsApi, type ForwardInput, type ForwardKind, type ForwardRecord, type ForwardStatus } from "./api";
import "./forwards.css";
import { passwordBus } from "../passwords/bus";
import { openInBrowser, writeClipboardText } from "../shell/clipboard";
import { loginServer, needsLogin } from "../servers/login";

/** 状态文字的色调 */
function tone(state: string): "ok" | "warn" | "danger" | "neutral" {
  if (state === "running") return "ok";
  if (state === "connecting" || state === "reconnecting" || state === "waiting_password") return "warn";
  if (state === "error") return "danger";
  return "neutral";
}

/** 错误的说明：端口被占用时带上占用的程序和进程号（异常 P2） */
function errorText(t: (k: string, o?: Record<string, unknown>) => string, e: unknown): string {
  // 同一台服务器上另一条已启用的转发用着这个端口（存储层检查出来的，没有占用程序的信息）
  if (isAppError(e) && e.code === "tunnel.port_in_use" && e.params.side && !e.params.process) {
    return t(e.params.side === "remote" ? "forwards.conflictRemote" : "forwards.conflictLocal", { port: e.params.port ?? "" });
  }
  const text = formatError(e);
  if (isAppError(e) && e.code === "tunnel.port_in_use" && e.params.process) {
    return `${text}${t("forwards.usedBy", { process: e.params.process, pid: e.params.pid ?? "?" })}`;
  }
  return text;
}

/**
 * 工具面板“服务器”页里的端口转发列表：开关、映射、状态、在浏览器打开；右键编辑、复制地址、删除。
 * @param serverId 当前服务器
 */
export function ForwardsSection({ serverId }: { serverId: string }) {
  const { t } = useTranslation();
  const [items, setItems] = useState<ForwardRecord[]>([]);
  // 列表读完之前不显示“还没有端口转发”，免得闪一下
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<{ title: string; text: string } | null>(null);
  const [editing, setEditing] = useState<ForwardRecord | "new" | null>(null);
  const [deleting, setDeleting] = useState<ForwardRecord | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const menu = useContextMenu(t("forwards.title"));
  // 用户刚开启（或刚新建）的转发及其所属服务器：如果随即进入“等待输入密码”，弹出那台服务器的密码框
  const justEnabled = useRef(new Map<string, string>());

  const load = () => getForwardsApi().then((api) => api.list(serverId)).then(
    (list) => { setItems(list); setLoaded(true); },
    (e: unknown) => { setLoaded(true); setError({ title: t("forwards.loadFailed"), text: formatError(e) }); },
  );
  /**
   * 要验证码的服务器（内置 SSH）：交互登录一次（问密码、验证码），之后后台转发用这次登录。
   * 转发已经因为“要验证码”停在出错状态时，登录后重新开启它。
   */
  const login = (owner: string, forwardId?: string) => {
    loginServer(owner).then(async () => {
      if (!forwardId) return;
      const api = await getForwardsApi();
      const current = (await api.list(serverId)).find((x) => x.id === forwardId);
      if (current?.enabled && current.status.state === "error") {
        await api.setEnabled(forwardId, false);
        const r = await api.setEnabled(forwardId, true);
        setItems((xs) => xs.map((x) => (x.id === r.id ? r : x)));
      }
    }).catch((e: unknown) => setError({ title: t("forwards.loginFailed"), text: formatError(e) }));
  };
  useEffect(() => { setItems([]); setLoaded(false); setError(null); void load(); }, [serverId]);
  // 状态推送：只更新状态，不重新读列表
  useEffect(() => {
    let off: (() => void) | null = null;
    let alive = true;
    void getForwardsApi().then((api) => api.onStatus((list: ForwardStatus[]) => {
      setItems((xs) => xs.map((x) => list.find((s) => s.id === x.id) ? { ...x, status: list.find((s) => s.id === x.id)! } : x));
      for (const st of list) {
        const owner = justEnabled.current.get(st.id);
        if (owner === undefined) continue;
        if (needsLogin(st.error) && (st.state === "waiting_password" || st.state === "error")) { justEnabled.current.delete(st.id); login(owner, st.id); }
        else if (st.state === "waiting_password") { justEnabled.current.delete(st.id); passwordBus.requestEntry(owner); }
        else if (st.state === "running" || st.state === "error") justEnabled.current.delete(st.id);
      }
    })).then((u) => { if (alive) off = u; else u(); });
    return () => { alive = false; off?.(); };
  }, []);

  const toggle = (f: ForwardRecord, enabled: boolean) => {
    setBusy(f.id);
    if (enabled) justEnabled.current.set(f.id, f.serverId);
    getForwardsApi().then((api) => api.setEnabled(f.id, enabled))
      .then((r) => {
        setItems((xs) => xs.map((x) => (x.id === r.id ? r : x)));
        setError(null);
        if (r.status.state === "waiting_password" && justEnabled.current.delete(r.id)) passwordBus.requestEntry(r.serverId);
      }, (e: unknown) => { justEnabled.current.delete(f.id); setError({ title: t("forwards.startFailed", { name: f.name }), text: errorText(t, e) }); void load(); })
      .finally(() => setBusy(null));
  };
  const address = (f: ForwardRecord) => (f.kind === "remote" ? `127.0.0.1:${f.remotePort}` : `127.0.0.1:${f.localPort}`);
  // 用 Windows 的默认浏览器打开（WebView2 里的 window.open 不会交给系统浏览器）
  const openBrowser = (f: ForwardRecord) => {
    void openInBrowser(`http://127.0.0.1:${f.localPort}/`).catch((e: unknown) => setError({ title: t("forwards.openFailed", { name: f.name }), text: errorText(t, e) }));
  };

  return (
    <section className="fwd" aria-label={t("forwards.title")}>
      <div className="fwd__head">
        <h3 className="ws__label">{t("forwards.title")}</h3>
        <Button size="sm" icon={<Plus size={13} />} onClick={() => setEditing("new")}>{t("forwards.new")}</Button>
      </div>
      {/* 出错时只占固定的一行（完整内容在悬停提示里），点开关时列表不跳 */}
      <p className={`fwd__error pl-ellipsis${error ? "" : " fwd__error--none"}`} role={error ? "alert" : undefined} title={error ? `${error.title}\n${error.text}` : undefined}>
        {error ? `${error.title}：${error.text}` : ""}
      </p>
      {loaded && items.length === 0 && !error && <p className="snip__hint">{t("forwards.empty")}</p>}
      {items.map((f) => (
        <div
          key={f.id}
          className="fwd__item"
          onContextMenu={(e) => menu.open(e, [
            { id: "toggle", label: f.enabled ? t("forwards.disable") : t("forwards.enable"), onSelect: () => toggle(f, !f.enabled) },
            ...(f.kind === "local" ? [{ id: "browser", label: t("forwards.openBrowser"), disabled: f.status.state !== "running", onSelect: () => openBrowser(f) }] : []),
            { id: "copy", label: t("forwards.copyAddress"), onSelect: () => void writeClipboardText(address(f)) },
            { kind: "separator" as const, id: "s1" },
            { id: "edit", label: t("forwards.edit"), onSelect: () => setEditing(f) },
            { kind: "separator" as const, id: "s2" },
            { id: "delete", label: t("common.delete"), danger: true, onSelect: () => setDeleting(f) },
          ])}
        >
          <Switch label={f.name} checked={f.enabled} disabled={busy === f.id} onChange={(v) => toggle(f, v)} />
          <div className="fwd__text">
            <span className="fwd__name pl-ellipsis">{f.name}</span>
            <span className="fwd__map mono pl-ellipsis">{describeForward(f, t)}</span>
            {/* 状态一直占一行；错误原因很长时截断，完整内容在悬停提示里 */}
            {(() => {
              const text = f.enabled
                ? `${t(`forwards.state.${f.status.state}`, { defaultValue: f.status.state })}${f.status.error && f.status.state !== "running" ? ` · ${errorText(t, f.status.error)}` : ""}`
                : t("forwards.state.stopped");
              return <span className={`fwd__state pl-ellipsis fwd__state--${f.enabled ? tone(f.status.state) : "neutral"}`} title={text}>{text}</span>;
            })()}
          </div>
          {/* “在浏览器打开”的位置一直留着，按钮出现时名称不被挤短 */}
          {f.kind === "local" && (
            <span className="fwd__open">
              {f.enabled && f.status.state === "running" && (
                <IconButton size="sm" label={t("forwards.openBrowser")} onClick={() => openBrowser(f)}><ExternalLink size={13} /></IconButton>
              )}
            </span>
          )}
          {f.enabled && needsLogin(f.status.error) && f.status.state !== "running" ? (
            <Button size="sm" onClick={() => login(f.serverId, f.id)}>{t("servers.login")}</Button>
          ) : f.enabled && f.status.state === "waiting_password" && (
            <Button size="sm" onClick={() => passwordBus.requestEntry(serverId)}>{t("passwords.enter")}</Button>
          )}
        </div>
      ))}
      <p className="snip__hint">{t("forwards.footer")}</p>
      {menu.node}
      {editing && (
        <ForwardDialog
          serverId={serverId}
          forward={editing === "new" ? null : editing}
          onSaved={(r) => {
            setEditing(null);
            setItems((xs) => (xs.some((x) => x.id === r.id) ? xs.map((x) => (x.id === r.id ? r : x)) : [...xs, r]));
            // 新建或修改后会立即开启：缺密码时直接弹框
            if (r.enabled) {
              if (r.status.state === "waiting_password") passwordBus.requestEntry(r.serverId);
              else justEnabled.current.set(r.id, r.serverId);
            }
          }}
          onClose={() => setEditing(null)}
        />
      )}
      {deleting && (
        <Dialog
          title={t("forwards.deleteTitle", { name: deleting.name })}
          danger
          onDismiss={() => setDeleting(null)}
          actions={<>
            <Button onClick={() => setDeleting(null)}>{t("common.cancel")}</Button>
            <Button variant="danger" onClick={() => {
              const id = deleting.id;
              setDeleting(null);
              getForwardsApi().then((api) => api.remove(id)).then(load, (e: unknown) => setError({ title: t("forwards.deleteFailed"), text: formatError(e) }));
            }}>{t("common.delete")}</Button>
          </>}
        >
          {describeForward(deleting, t)}
        </Dialog>
      )}
    </section>
  );
}

/** 端口号输入：1–65535 的整数，否则为 null */
function port(v: string): number | null {
  const n = Number(v);
  return Number.isInteger(n) && n >= 1 && n <= 65535 ? n : null;
}

/** 新建或修改端口转发 */
function ForwardDialog({ serverId, forward, onSaved, onClose }: {
  serverId: string;
  forward: ForwardRecord | null;
  onSaved: (r: ForwardRecord) => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [kind, setKind] = useState<ForwardKind>(forward?.kind ?? "local");
  const [name, setName] = useState(forward?.name ?? "");
  const [localPort, setLocalPort] = useState(forward ? String(forward.localPort) : "");
  const [remoteHost, setRemoteHost] = useState(forward?.remoteHost ?? "127.0.0.1");
  const [remotePort, setRemotePort] = useState(forward?.remotePort != null ? String(forward.remotePort) : "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const lp = port(localPort);
  const rp = port(remotePort);
  const valid = !!name.trim() && lp !== null && (kind === "dynamic" || rp !== null) && (kind !== "local" || !!remoteHost.trim());
  const save = () => {
    if (!valid || lp === null) return;
    const input: ForwardInput = {
      id: forward?.id ?? null,
      serverId,
      name: name.trim(),
      kind,
      localPort: lp,
      remoteHost: kind === "local" ? remoteHost.trim() : null,
      remotePort: kind === "dynamic" ? null : rp,
      enabled: forward ? forward.enabled : true,
      sortOrder: forward?.sortOrder ?? 0,
    };
    setBusy(true);
    setError(null);
    getForwardsApi().then((api) => api.save(input)).then(onSaved, (e: unknown) => { setError(errorText(t, e)); setBusy(false); });
  };
  return (
    <Dialog
      title={forward ? t("forwards.editTitle") : t("forwards.newTitle")}
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="primary" onClick={save} disabled={busy || !valid}>{forward ? t("common.save") : t("forwards.createAndStart")}</Button>
      </>}
    >
      <div className="snip-form">
        <SelectField label={t("forwards.kind")} value={kind} onChange={(v) => setKind(v as ForwardKind)} options={[
          { value: "local", label: t("forwards.kindLocal") },
          { value: "remote", label: t("forwards.kindRemote") },
          { value: "dynamic", label: t("forwards.kindDynamic") },
        ]} />
        <p className="snip__hint">{t(`forwards.kindHint.${kind}`)}</p>
        <TextField label={t("forwards.name")} value={name} maxLength={60} onChange={(e) => setName(e.target.value)} placeholder={kind === "dynamic" ? t("forwards.namePlaceholderSocks") : t("forwards.namePlaceholder")} autoFocus />
        {kind === "remote" && (
          <TextField label={t("forwards.serverPort")} value={remotePort} inputMode="numeric" onChange={(e) => setRemotePort(e.target.value)} placeholder={t("forwards.portPlaceholder", { port: 8080 })} />
        )}
        <TextField label={kind === "remote" ? t("forwards.localTargetPort") : t("forwards.localPort")} value={localPort} inputMode="numeric" onChange={(e) => setLocalPort(e.target.value)} placeholder={t("forwards.portPlaceholder", { port: kind === "dynamic" ? 1080 : 6006 })} />
        {kind === "local" && (
          <div className="fwd-form__row">
            <TextField label={t("forwards.remoteHost")} value={remoteHost} spellCheck={false} onChange={(e) => setRemoteHost(e.target.value)} />
            <TextField label={t("forwards.remotePort")} value={remotePort} inputMode="numeric" onChange={(e) => setRemotePort(e.target.value)} placeholder={t("forwards.portPlaceholder", { port: 6006 })} />
          </div>
        )}
        {kind === "dynamic" && <Banner tone="warn" title={t("forwards.socksRiskTitle")}>{t("forwards.socksRisk")}</Banner>}
        {lp !== null && lp < 1024 && kind !== "remote" && <p className="snip__hint">{t("forwards.lowPort")}</p>}
        {kind === "remote" && rp !== null && rp < 1024 && <p className="snip__hint snip__hint--warn">{t("forwards.lowRemotePort")}</p>}
        {error && <Banner tone="danger" title={t("forwards.saveFailed")}>{error}</Banner>}
      </div>
    </Dialog>
  );
}
