import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, Copy, Pencil, Trash2, X } from "lucide-react";
import { Button, Checkbox, IconButton, Select, Switch } from "../../components";
import { formatError } from "../../errors";
import { writeClipboardText } from "../shell/clipboard";
import { getMobileApi, type MobileDevice, type MobileInvite, type MobileStatus } from "./api";
import { deviceSummary } from "./userAgent";
import "./mobile.css";

function ago(t: (k: string, o?: Record<string, unknown>) => string, ms: number | null): string {
  if (ms === null) return t("mobileSettings.never");
  const min = Math.round((Date.now() - ms) / 60000);
  if (min < 1) return t("mobileSettings.justNow");
  if (min < 60) return t("mobileSettings.minutesAgo", { count: min });
  const h = Math.round(min / 60);
  return h < 48 ? t("mobileSettings.hoursAgo", { count: h }) : t("mobileSettings.daysAgo", { count: Math.round(h / 24) });
}

/** 配对日期：今年的只写月日，往年的带年份 */
function day(ms: number): string {
  const d = new Date(ms);
  const opts: Intl.DateTimeFormatOptions = d.getFullYear() === new Date().getFullYear() ? { month: "numeric", day: "numeric" } : { year: "numeric", month: "numeric", day: "numeric" };
  return d.toLocaleDateString(undefined, opts);
}

/** 一台已配对的手机：名字（可改）、系统和浏览器、最近的来源 IP、权限 */
function DeviceRow({ d, onPerm, onRevoke, onRename }: {
  d: MobileDevice;
  onPerm: (key: keyof MobileDevice["permissions"], value: boolean) => void;
  onRevoke: () => void;
  onRename: (name: string) => Promise<boolean>;
}) {
  const { t } = useTranslation();
  const [editing, setEditing] = useState<string | null>(null);
  const summary = deviceSummary(d.userAgent);
  const save = async () => {
    if (editing === null) return;
    const name = editing.trim();
    if (!name || name === d.name || (await onRename(name))) setEditing(null);
  };
  return (
    <div className="mobile-dev">
      <div className="mobile-dev__head">
        {editing === null ? (
          <>
            <b className="mobile-dev__name">{d.name}</b>
            <IconButton size="sm" label={t("mobileSettings.rename")} onClick={() => setEditing(d.name)}><Pencil size={13} /></IconButton>
          </>
        ) : (
          <>
            <input className="pl-input mobile-dev__input" aria-label={t("mobileSettings.rename")} value={editing} maxLength={80} autoFocus
              onChange={(e) => setEditing(e.target.value)}
              onKeyDown={(e) => { if (e.key === "Enter") void save(); else if (e.key === "Escape") setEditing(null); }} />
            <IconButton size="sm" label={t("common.save")} onClick={() => void save()}><Check size={13} /></IconButton>
            <IconButton size="sm" label={t("common.cancel")} onClick={() => setEditing(null)}><X size={13} /></IconButton>
          </>
        )}
        <span className="mobile-dev__spacer" />
        <IconButton size="sm" label={t("mobileSettings.revoke")} onClick={onRevoke}><Trash2 size={13} /></IconButton>
      </div>
      <span className="set-row__hint">
        {[summary, d.lastIp, t("mobileSettings.pairedOn", { date: day(d.createdMs) }), t("mobileSettings.lastSeen", { when: ago(t, d.lastSeenMs) })].filter(Boolean).join("  ·  ")}
      </span>
      <div className="mobile-dev__perms">
        <Checkbox label={t("mobileSettings.perm.view")} checked={d.permissions.view} onChange={(e) => onPerm("view", e.target.checked)} />
        <Checkbox label={t("mobileSettings.perm.input")} checked={d.permissions.input} onChange={(e) => onPerm("input", e.target.checked)} />
        <Checkbox label={t("mobileSettings.perm.confirm")} checked={d.permissions.confirm} onChange={(e) => onPerm("confirm", e.target.checked)} />
      </div>
    </div>
  );
}

/**
 * 设置页“手机”：打开手机入口（选监听地址：Tailscale / 局域网），用二维码配对手机，
 * 管理已配对的手机（每台的权限、改名、撤销）。只监听选定的地址，不对所有网卡开放。
 * 配对用的二维码只在点“配对新手机”后出现；配对成功、过期或点关闭后收起。
 */
export function MobileSection() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<MobileStatus | null>(null);
  const [devices, setDevices] = useState<MobileDevice[]>([]);
  const [invite, setInvite] = useState<MobileInvite | null>(null);
  /** 刚配对成功的手机名（二维码收起后显示一句） */
  const [paired, setPaired] = useState<string | null>(null);
  const [copied, setCopied] = useState<"code" | "link" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());
  /** 生成当前邀请码的时间：之后出现的新手机就是用它配对的 */
  const inviteAt = useRef(0);

  const reload = () => {
    void getMobileApi().then(async (api) => {
      setStatus(await api.status());
      const list = await api.devices();
      setDevices(list);
      // 用这个邀请码配对成功了：收起二维码
      const fresh = inviteAt.current ? list.find((d) => d.createdMs >= inviteAt.current) : undefined;
      if (fresh) { inviteAt.current = 0; setInvite(null); setPaired(fresh.name); }
    }).catch((e: unknown) => setError(formatError(e)));
  };
  useEffect(() => {
    reload();
    let off: (() => void) | undefined;
    let alive = true;
    void getMobileApi().then((api) => api.onChanged(reload)).then((u) => { if (alive) off = u; else u(); });
    return () => { alive = false; off?.(); };
  }, []);
  // 邀请码倒计时；过期就收起
  useEffect(() => {
    if (!invite) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [invite]);
  const left = invite ? Math.max(0, Math.round((invite.expiresAtMs - now) / 1000)) : 0;
  useEffect(() => {
    if (invite && left === 0) { inviteAt.current = 0; setInvite(null); }
  }, [invite, left]);

  const configure = async (enabled: boolean, bind: string | null) => {
    setError(null);
    try {
      const s = await (await getMobileApi()).configure(enabled, bind, status?.port ?? 48600);
      setStatus(s);
      if (!s.running) { inviteAt.current = 0; setInvite(null); }
    } catch (e) {
      setError(formatError(e));
    }
  };
  const newInvite = async () => {
    setError(null);
    setPaired(null);
    setCopied(null);
    try {
      // 留几秒余量：电脑和后端记的时间来自同一个时钟，但取整到秒
      inviteAt.current = Date.now() - 2000;
      const created = await (await getMobileApi()).invite();
      // 倒计时从现在算起（否则第一秒会用上一次记下的时间，显示成 10:02 这样）
      setNow(Date.now());
      setInvite(created);
    } catch (e) { inviteAt.current = 0; setError(formatError(e)); }
  };
  const closeInvite = () => {
    inviteAt.current = 0;
    setInvite(null);
    void getMobileApi().then((api) => api.cancelInvite()).catch((e: unknown) => setError(formatError(e)));
  };
  const copy = (what: "code" | "link", text: string) => {
    void writeClipboardText(text).then(() => setCopied(what), (e: unknown) => setError(formatError(e)));
  };
  const setPerm = async (d: MobileDevice, key: keyof MobileDevice["permissions"], value: boolean) => {
    const p = { ...d.permissions, [key]: value };
    setDevices((l) => l.map((x) => (x.id === d.id ? { ...x, permissions: p } : x)));
    try { await (await getMobileApi()).setPermissions(d.id, p); } catch (e) { setError(formatError(e)); reload(); }
  };
  const rename = async (d: MobileDevice, name: string): Promise<boolean> => {
    setError(null);
    try {
      await (await getMobileApi()).rename(d.id, name);
      setDevices((l) => l.map((x) => (x.id === d.id ? { ...x, name } : x)));
      return true;
    } catch (e) { setError(formatError(e)); return false; }
  };
  const revoke = async (id: string | null) => {
    try {
      const api = await getMobileApi();
      if (id) await api.revoke(id); else await api.revokeAll();
      reload();
    } catch (e) { setError(formatError(e)); }
  };

  if (!status) return <div className="set-card"><p className="set-row__hint mobile-pad">{error ?? t("common.loading")}</p></div>;
  const options = status.addresses.map((a) => ({ value: a.ip, label: `${a.ip}  ·  ${t(`mobileSettings.kind.${a.kind}`, { defaultValue: a.name })}` }));
  const active = invite && left > 0 ? invite : null;
  return (
    <div className="set-card mobile-set">
      <div className="set-row">
        <span className="set-row__text">
          <span>{t("mobileSettings.enable")}</span>
          <span className="set-row__hint">{t("mobileSettings.enableHint")}</span>
        </span>
        <span className="set-row__control">
          <Switch label={t("mobileSettings.enable")} checked={status.enabled} onChange={(v) => void configure(v, status.bind ?? status.addresses.find((a) => a.kind === "tailscale")?.ip ?? null)} />
        </span>
      </div>
      <div className="set-row">
        <span className="set-row__text">
          <span>{t("mobileSettings.address")}</span>
          <span className="set-row__hint">{t("mobileSettings.addressHint")} {t("mobileSettings.firewallHint")}</span>
        </span>
        <span className="set-row__control mobile-set__address">
          {/* 每次点开都重新读本机地址：刚连上 Tailscale 时也能马上选到 */}
          <Select label={t("mobileSettings.address")} value={status.bind ?? ""} options={[{ value: "", label: t("mobileSettings.pick") }, ...options]}
            onOpen={reload} onChange={(v) => void configure(status.enabled, v || null)} />
        </span>
      </div>
      {/* 状态和操作出错都显示在这一行（固定位置，不把下面的内容往下挤） */}
      <p className={`mobile-set__state${error ? " mobile-set__state--error" : status.running ? " mobile-set__state--on" : ""}`} role="status">
        {error ?? (status.running && status.url ? t("mobileSettings.running", { url: status.url }) : status.enabled ? (status.error ? formatError(status.error) : t("mobileSettings.notRunning")) : t("mobileSettings.off"))}
      </p>

      {status.running && (active ? (
        <div className="mobile-set__pair">
          {/* 二维码是后端用固定库生成的 SVG（只有方块），不含外部内容 */}
          <div className="mobile-set__qr-img" aria-label={t("mobileSettings.qr")} dangerouslySetInnerHTML={{ __html: active.qrSvg }} />
          <div className="mobile-set__pair-text">
            <span>{t("mobileSettings.scan")}</span>
            <span className="mobile-set__code-row">
              <span className="mono mobile-set__code">{active.code}</span>
              <IconButton size="sm" label={copied === "code" ? t("mobileSettings.copied") : t("mobileSettings.copyCode")} onClick={() => copy("code", active.code)}>
                {copied === "code" ? <Check size={14} /> : <Copy size={14} />}
              </IconButton>
            </span>
            <span className="set-row__hint">{t("mobileSettings.expires", { min: Math.floor(left / 60), sec: String(left % 60).padStart(2, "0") })}</span>
            <span className="mobile-set__code-row">
              <span className="set-row__hint mono mobile-set__url">{active.url}</span>
              <IconButton size="sm" label={copied === "link" ? t("mobileSettings.copied") : t("mobileSettings.copyLink")} onClick={() => copy("link", active.url)}>
                {copied === "link" ? <Check size={14} /> : <Copy size={14} />}
              </IconButton>
            </span>
            <span className="mobile-set__actions">
              <Button size="sm" onClick={() => void newInvite()}>{t("mobileSettings.newCode")}</Button>
            </span>
          </div>
          <IconButton className="mobile-set__close" size="sm" label={t("mobileSettings.closeInvite")} onClick={closeInvite}><X size={14} /></IconButton>
        </div>
      ) : (
        <div className="mobile-set__pair mobile-set__pair--idle">
          <span className="set-row__hint">{paired ? t("mobileSettings.pairedDone", { name: paired }) : t("mobileSettings.pairHint")}</span>
          <Button size="sm" onClick={() => void newInvite()}>{t("mobileSettings.pair")}</Button>
        </div>
      ))}

      <div className="mobile-set__devices">
        <div className="mobile-set__devices-head">
          <span>{t("mobileSettings.devices")}</span>
          {devices.length > 0 && <Button size="sm" variant="danger-ghost" onClick={() => void revoke(null)}>{t("mobileSettings.revokeAll")}</Button>}
        </div>
        {devices.length === 0 && <p className="set-row__hint">{t("mobileSettings.noDevices")}</p>}
        {devices.map((d) => (
          <DeviceRow key={d.id} d={d} onPerm={(k, v) => void setPerm(d, k, v)} onRevoke={() => void revoke(d.id)} onRename={(name) => rename(d, name)} />
        ))}
        <p className="set-row__hint">{t("mobileSettings.safety")}</p>
      </div>
    </div>
  );
}
