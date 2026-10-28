import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { QrCode, Trash2 } from "lucide-react";
import { Button, Checkbox, Select, Switch } from "../../components";
import { formatError } from "../../errors";
import { getMobileApi, type MobileDevice, type MobileInvite, type MobileStatus } from "./api";
import "./mobile.css";

function ago(t: (k: string, o?: Record<string, unknown>) => string, ms: number | null): string {
  if (ms === null) return t("mobileSettings.never");
  const min = Math.round((Date.now() - ms) / 60000);
  if (min < 1) return t("mobileSettings.justNow");
  if (min < 60) return t("mobileSettings.minutesAgo", { count: min });
  const h = Math.round(min / 60);
  return h < 48 ? t("mobileSettings.hoursAgo", { count: h }) : t("mobileSettings.daysAgo", { count: Math.round(h / 24) });
}

/**
 * 设置页“手机”：打开手机入口（选监听地址：Tailscale / 局域网），用二维码配对手机，
 * 管理已配对的手机（每台的权限、撤销）。只监听选定的地址，不对所有网卡开放。
 */
export function MobileSection() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<MobileStatus | null>(null);
  const [devices, setDevices] = useState<MobileDevice[]>([]);
  const [invite, setInvite] = useState<MobileInvite | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());

  const reload = () => {
    void getMobileApi().then(async (api) => {
      setStatus(await api.status());
      setDevices(await api.devices());
    }).catch((e: unknown) => setError(formatError(e)));
  };
  useEffect(() => {
    reload();
    let off: (() => void) | undefined;
    let alive = true;
    void getMobileApi().then((api) => api.onChanged(reload)).then((u) => { if (alive) off = u; else u(); });
    return () => { alive = false; off?.(); };
  }, []);
  // 邀请码倒计时
  useEffect(() => {
    if (!invite) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [invite]);
  const left = invite ? Math.max(0, Math.round((invite.expiresAtMs - now) / 1000)) : 0;

  const configure = async (enabled: boolean, bind: string | null) => {
    setError(null);
    try {
      const s = await (await getMobileApi()).configure(enabled, bind, status?.port ?? 48600);
      setStatus(s);
      if (!s.running) setInvite(null);
    } catch (e) {
      setError(formatError(e));
    }
  };
  const newInvite = async () => {
    setError(null);
    try { setInvite(await (await getMobileApi()).invite()); } catch (e) { setError(formatError(e)); }
  };
  const setPerm = async (d: MobileDevice, key: keyof MobileDevice["permissions"], value: boolean) => {
    const p = { ...d.permissions, [key]: value };
    setDevices((l) => l.map((x) => (x.id === d.id ? { ...x, permissions: p } : x)));
    try { await (await getMobileApi()).setPermissions(d.id, p); } catch (e) { setError(formatError(e)); reload(); }
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
          <Select label={t("mobileSettings.address")} value={status.bind ?? ""} options={[{ value: "", label: t("mobileSettings.pick") }, ...options]}
            onChange={(v) => void configure(status.enabled, v || null)} />
        </span>
      </div>
      {/* 状态和操作出错都显示在这一行（固定位置，不把下面的内容往下挤） */}
      <p className={`mobile-set__state${error ? " mobile-set__state--error" : status.running ? " mobile-set__state--on" : ""}`} role="status">
        {error ?? (status.running && status.url ? t("mobileSettings.running", { url: status.url }) : status.enabled ? (status.error ? formatError(status.error) : t("mobileSettings.notRunning")) : t("mobileSettings.off"))}
      </p>

      {status.running && (
        <div className="mobile-set__pair">
          <div className="mobile-set__qr" aria-label={t("mobileSettings.qr")}>
            {invite && left > 0
              // 二维码是后端用固定库生成的 SVG（只有方块），不含外部内容
              ? <div className="mobile-set__qr-img" dangerouslySetInnerHTML={{ __html: invite.qrSvg }} />
              : <QrCode size={40} aria-hidden="true" />}
          </div>
          <div className="mobile-set__pair-text">
            {invite && left > 0 ? (
              <>
                <span>{t("mobileSettings.scan")}</span>
                <span className="mono mobile-set__code">{invite.code}</span>
                <span className="set-row__hint">{t("mobileSettings.expires", { min: Math.floor(left / 60), sec: String(left % 60).padStart(2, "0") })}</span>
                <span className="set-row__hint mono">{invite.url}</span>
              </>
            ) : (
              <span className="set-row__hint">{t("mobileSettings.pairHint")}</span>
            )}
            <Button size="sm" onClick={() => void newInvite()}>{invite && left > 0 ? t("mobileSettings.newCode") : t("mobileSettings.pair")}</Button>
          </div>
        </div>
      )}

      <div className="mobile-set__devices">
        <div className="mobile-set__devices-head">
          <span>{t("mobileSettings.devices")}</span>
          {devices.length > 0 && <Button size="sm" variant="danger-ghost" onClick={() => void revoke(null)}>{t("mobileSettings.revokeAll")}</Button>}
        </div>
        {devices.length === 0 && <p className="set-row__hint">{t("mobileSettings.noDevices")}</p>}
        {devices.map((d) => (
          <div key={d.id} className="mobile-dev">
            <div className="mobile-dev__head">
              <b>{d.name}</b>
              <span className="set-row__hint">{t("mobileSettings.lastSeen", { when: ago(t, d.lastSeenMs) })}</span>
              <button type="button" className="mcp-cmd__copy" title={t("mobileSettings.revoke")} aria-label={t("mobileSettings.revoke")} onClick={() => void revoke(d.id)}><Trash2 size={13} /></button>
            </div>
            <div className="mobile-dev__perms">
              <Checkbox label={t("mobileSettings.perm.view")} checked={d.permissions.view} onChange={(e) => void setPerm(d, "view", e.target.checked)} />
              <Checkbox label={t("mobileSettings.perm.input")} checked={d.permissions.input} onChange={(e) => void setPerm(d, "input", e.target.checked)} />
              <Checkbox label={t("mobileSettings.perm.confirm")} checked={d.permissions.confirm} onChange={(e) => void setPerm(d, "confirm", e.target.checked)} />
            </div>
          </div>
        ))}
        <p className="set-row__hint">{t("mobileSettings.safety")}</p>
      </div>
    </div>
  );
}
