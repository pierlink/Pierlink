import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Segmented } from "../../components";
import { formatError } from "../../errors";
import { DEFAULT_BYPASS, getProxy, saveProxy, systemProxy, testProxy, type ProxyKind, type ProxyMode, type ProxySettings, type SystemProxy } from "./proxyApi";

/** 名单文本按行拆开，去掉空行 */
function lines(text: string): string[] {
  return text.split(/\r?\n/).map((x) => x.trim()).filter(Boolean);
}

/** 两份设置是否相同（地址两头的空白不算） */
function same(a: ProxySettings, b: ProxySettings): boolean {
  return a.mode === b.mode && a.kind === b.kind && a.host.trim() === b.host.trim() && a.port === b.port && a.bypass.join("\n") === b.bypass.join("\n");
}

function systemText(t: (k: string) => string, s: SystemProxy | null): string {
  if (!s) return "…";
  if (s.kind === "none") return t("settings.proxy.systemNone");
  if (s.kind === "pac") return t("settings.proxy.systemPac");
  const host = s.host.includes(":") ? `[${s.host}]` : s.host;
  return `${s.protocol === "socks5" ? "SOCKS5" : "HTTP"} ${host}:${s.port}`;
}

/**
 * 设置页“网络”：代理（不使用 / 跟随系统 / 自定义 HTTP、SOCKS5）。
 * 改完点“保存”才生效（地址填到一半不会被套用）；“测试代理”按当前填的内容经代理连一下 github.com。
 */
export function ProxySection() {
  const { t } = useTranslation();
  const [saved, setSaved] = useState<ProxySettings | null>(null);
  const [draft, setDraft] = useState<ProxySettings | null>(null);
  const [portText, setPortText] = useState("");
  const [bypassText, setBypassText] = useState("");
  const [system, setSystem] = useState<SystemProxy | null>(null);
  const [status, setStatus] = useState<{ tone: "ok" | "danger" | "muted"; text: string } | null>(null);
  const [busy, setBusy] = useState<"test" | "save" | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  const show = (s: ProxySettings) => {
    setSaved(s);
    setDraft(s);
    setPortText(s.port ? String(s.port) : "");
    setBypassText(s.bypass.join("\n"));
  };
  useEffect(() => { getProxy().then(show, (e: unknown) => setLoadError(formatError(e))); }, []);
  // 选“跟随系统”时显示系统现在的代理（每次切过来现读）
  useEffect(() => {
    if (draft?.mode !== "system") return;
    let alive = true;
    setSystem(null);
    systemProxy().then((s) => { if (alive) setSystem(s); }, () => { if (alive) setSystem({ kind: "none" }); });
    return () => { alive = false; };
  }, [draft?.mode]);

  if (loadError) return <Banner tone="danger" title={loadError} />;
  if (!draft || !saved) return null;

  const current: ProxySettings = { ...draft, port: portText ? Number(portText) : null, bypass: lines(bypassText) };
  const dirty = !same(current, saved);
  const edit = (patch: Partial<ProxySettings>) => { setDraft({ ...draft, ...patch }); setStatus(null); };

  const save = async () => {
    setBusy("save");
    setStatus(null);
    try {
      show(await saveProxy(current));
      setStatus({ tone: "ok", text: t("settings.proxy.saved") });
    } catch (e) {
      setStatus({ tone: "danger", text: formatError(e) });
    } finally {
      setBusy(null);
    }
  };
  const test = async () => {
    setBusy("test");
    setStatus({ tone: "muted", text: t("settings.proxy.testing") });
    try {
      const ms = await testProxy(current);
      setStatus({ tone: "ok", text: t("settings.proxy.testOk", { ms }) });
    } catch (e) {
      setStatus({ tone: "danger", text: formatError(e) });
    } finally {
      setBusy(null);
    }
  };

  return (
    <>
      <div className="set-card">
        <div className="set-row">
          <span className="set-row__text">
            <span>{t("settings.proxy.mode")}</span>
            <span className="set-row__hint">{t("settings.proxy.modeHint")}</span>
          </span>
          <span className="set-row__control">
            <Segmented<ProxyMode> label={t("settings.proxy.mode")} value={draft.mode} onChange={(mode) => edit({ mode })}
              options={(["off", "system", "custom"] as const).map((m) => ({ value: m, label: t(`settings.proxy.modes.${m}`) }))} />
          </span>
        </div>
        {draft.mode === "system" && (
          <div className="set-row">
            <span className="set-row__text"><span>{t("settings.proxy.system")}</span></span>
            <span className="set-row__control"><span className="mono proxy-system">{systemText(t, system)}</span></span>
          </div>
        )}
        {draft.mode === "custom" && (
          <>
            <div className="set-row">
              <span className="set-row__text"><span>{t("settings.proxy.protocol")}</span></span>
              <span className="set-row__control">
                <Segmented<ProxyKind> label={t("settings.proxy.protocol")} value={draft.kind} onChange={(kind) => edit({ kind })}
                  options={[{ value: "http", label: "HTTP" }, { value: "socks5", label: "SOCKS5" }]} />
              </span>
            </div>
            <div className="set-row">
              <span className="set-row__text">
                <span>{t("settings.proxy.address")}</span>
                <span className="set-row__hint">{t("settings.proxy.addressHint")}</span>
              </span>
              <span className="set-row__control proxy-addr">
                <input className="pl-input mono proxy-addr__host" type="text" aria-label={t("settings.proxy.host")} placeholder="127.0.0.1" value={draft.host}
                  spellCheck={false} autoComplete="off" maxLength={255} onChange={(e) => edit({ host: e.target.value })} />
                <span className="proxy-addr__sep" aria-hidden>:</span>
                <input className="pl-input mono proxy-addr__port" type="text" inputMode="numeric" aria-label={t("settings.proxy.port")} placeholder="7890" value={portText}
                  autoComplete="off" onChange={(e) => { setPortText(e.target.value.replace(/[^0-9]/g, "").slice(0, 5)); setStatus(null); }} />
              </span>
            </div>
          </>
        )}
      </div>
      {draft.mode === "custom" && (
        <div className="set-card set-rules proxy-bypass">
          <label className="wiz-field">
            <span className="wiz-field__label">{t("settings.proxy.bypass", { count: current.bypass.length })}</span>
            <textarea className="pl-input mono wiz-rules" rows={6} spellCheck={false} value={bypassText} onChange={(e) => { setBypassText(e.target.value); setStatus(null); }} />
          </label>
          <div className="set-rules__actions">
            <span className="sync-muted">{t("settings.proxy.bypassHint")}</span>
            <span style={{ flex: 1 }} />
            <Button size="sm" variant="ghost" onClick={() => { setBypassText(DEFAULT_BYPASS.join("\n")); setStatus(null); }}>{t("settings.proxy.bypassReset")}</Button>
          </div>
        </div>
      )}
      <div className="proxy-actions">
        <span className={`proxy-status proxy-status--${status?.tone ?? (dirty ? "muted" : "none")}`} role="status" title={status?.text}>
          {status?.text ?? (dirty ? t("settings.proxy.unsaved") : "")}
        </span>
        <Button size="sm" variant="secondary" disabled={draft.mode === "off" || busy !== null} onClick={() => void test()}>{t("settings.proxy.test")}</Button>
        <Button size="sm" variant="primary" disabled={!dirty || busy !== null} onClick={() => void save()}>{t("common.save")}</Button>
      </div>
    </>
  );
}
