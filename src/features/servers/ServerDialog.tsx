import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Dialog, Segmented, SuggestInput, TextField } from "../../components";
import { formatError, isAppError } from "../../errors";
import type { ServerInputView, ServerRecord } from "./api";
import "./servers.css";

type Auth = ServerInputView["authMethod"];
type Engine = ServerInputView["connectionEngine"];

interface Props {
  /** 修改时传原记录，新建时为 null */
  server: ServerRecord | null;
  groups: string[];
  onSave: (input: ServerInputView) => Promise<void>;
  onClose: () => void;
}

/** 从 “用户@主机:端口” 拆出各部分，方便用户直接粘贴一整串 */
function splitTarget(text: string): { user?: string; host: string; port?: number } | null {
  const m = /^(?:([^@\s]+)@)?(\[[^\]]+\]|[^:\s]+)(?::(\d{1,5}))?$/.exec(text.trim());
  if (!m) return null;
  const port = m[3] ? Number(m[3]) : undefined;
  return { user: m[1], host: m[2].replace(/^\[|\]$/g, ""), port: port && port <= 65535 ? port : undefined };
}

/** 新建或修改服务器（应用内新建的服务器；导入自 ssh config 的服务器在那边修改） */
export function ServerDialog({ server, groups, onSave, onClose }: Props) {
  const { t } = useTranslation();
  const [form, setForm] = useState<ServerInputView>(() => ({
    id: server?.id ?? null,
    name: server?.name ?? "",
    groupName: server?.groupName ?? null,
    host: server?.host ?? "",
    port: server?.port ?? 22,
    username: server?.username ?? null,
    authMethod: server?.authMethod ?? "agent",
    identityFile: server?.identityFile ?? null,
    loginShell: server?.loginShell ?? null,
    notes: server?.notes ?? "",
    connectionEngine: server?.connectionEngine ?? "builtin",
  }));
  const [portText, setPortText] = useState(String(form.port));
  const [nameTouched, setNameTouched] = useState(!!server);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<{ field: string | null; text: string } | null>(null);

  const set = (patch: Partial<ServerInputView>) => { setForm((f) => ({ ...f, ...patch })); setError(null); };
  const fieldError = (f: string) => (error?.field === f ? error.text : null);

  const onHostChange = (value: string) => {
    set({ host: value, ...(nameTouched ? {} : { name: value.trim() }) });
  };
  // 地址框里写了 user@host:port 时拆开。只在粘贴后或离开地址框时拆：
  // 逐字输入时如果每次按键都拆，打到端口第一位就会被拆走，后面的数字会接到主机名上
  const splitHost = (value: string) => {
    const parts = value.includes("@") || /:\d+$/.test(value) ? splitTarget(value) : null;
    if (!parts) return;
    set({ host: parts.host, ...(parts.user ? { username: parts.user } : {}), ...(parts.port ? { port: parts.port } : {}), ...(nameTouched ? {} : { name: parts.host }) });
    if (parts.port) setPortText(String(parts.port));
  };

  const submit = async () => {
    // 输完直接回车保存（没离开地址框）时，也按 user@host:port 拆开
    const parts = form.host.includes("@") || /:\d+$/.test(form.host.trim()) ? splitTarget(form.host) : null;
    const host = parts?.host ?? form.host;
    const username = parts?.user ?? form.username;
    const port = parts?.port ?? Number(portText);
    if (!Number.isInteger(port) || port < 1 || port > 65535) { setError({ field: "port", text: t("servers.invalid.port") }); return; }
    if (parts) splitHost(form.host);
    setBusy(true);
    try {
      await onSave({ ...form, host, username, port, name: nameTouched ? form.name : host });
      onClose();
    } catch (e) {
      const field = isAppError(e) && e.code === "server.invalid_field" ? e.params.field ?? null : null;
      setError({ field, text: field ? t(`servers.invalid.${field}`, { defaultValue: formatError(e) }) : formatError(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      title={server ? t("servers.editTitle", { name: server.name }) : t("servers.addTitle")}
      size="lg"
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="primary" disabled={busy || !form.host.trim() || !form.name.trim()} onClick={() => void submit()}>{t("common.save")}</Button>
      </>}
    >
      <form className="srv-form" onSubmit={(e) => { e.preventDefault(); void submit(); }}>
        <div className="srv-form__row srv-form__row--host">
          <TextField label={t("servers.host")} className="pl-input mono" autoFocus placeholder={t("servers.hostPlaceholder")} value={form.host} onChange={(e) => onHostChange(e.target.value)} onBlur={(e) => splitHost(e.target.value)}
            onPaste={(e) => { const text = e.clipboardData.getData("text/plain"); if (splitTarget(text) && (text.includes("@") || /:\d+$/.test(text.trim()))) { e.preventDefault(); splitHost(text); } }} aria-invalid={!!fieldError("host")} />
          <TextField label={t("servers.port")} className="pl-input mono" inputMode="numeric" value={portText} onChange={(e) => { setPortText(e.target.value.replace(/[^0-9]/g, "").slice(0, 5)); setError(null); }} aria-invalid={!!fieldError("port")} />
        </div>
        {fieldError("host") || fieldError("port")
          ? <span className="srv-form__err" role="alert">{fieldError("host") ?? fieldError("port")}</span>
          : <span className="srv-form__hint">{t("servers.hostHint")}</span>}
        <div className="srv-form__row">
          <TextField label={t("servers.username")} className="pl-input mono" placeholder={t("servers.usernamePlaceholder")} value={form.username ?? ""} onChange={(e) => set({ username: e.target.value || null })} aria-invalid={!!fieldError("username")} />
          <TextField label={t("servers.name")} value={form.name} onChange={(e) => { setNameTouched(true); set({ name: e.target.value }); }} aria-invalid={!!fieldError("name")} />
        </div>
        {(fieldError("username") || fieldError("name")) && <span className="srv-form__err" role="alert">{fieldError("username") ?? fieldError("name")}</span>}

        <div className="wiz-field">
          <span className="wiz-field__label">{t("servers.auth")}</span>
          <Segmented<Auth>
            label={t("servers.auth")}
            value={form.authMethod}
            onChange={(authMethod) => set({ authMethod })}
            // 专用密钥只能经“专用密钥…”安装得到；已经是专用密钥的服务器可以保持或改回其他方式
            options={(["agent", "key", "password", ...(server?.authMethod === "dedicated_key" ? ["dedicated_key"] : [])] as Auth[]).map((v) => ({ value: v, label: t(`servers.authMethod.${v}`) }))}
          />
          <span className="wiz-hint">{t(`servers.authHint.${form.authMethod}`)}</span>
        </div>

        <div className="wiz-field">
          <span className="wiz-field__label">{t("servers.engine")}</span>
          <Segmented<Engine>
            label={t("servers.engine")}
            value={form.connectionEngine}
            onChange={(connectionEngine) => set({ connectionEngine })}
            options={(["builtin", "openssh"] as Engine[]).map((v) => ({ value: v, label: t(`servers.engineName.${v}`) }))}
          />
          <span className="wiz-hint">{t(`servers.engineHint.${form.connectionEngine}`)}</span>
          {/* 内置 SSH 要明确的用户名；没填（地址框里也没写 用户@）时这台服务器实际会改用系统 OpenSSH */}
          {form.connectionEngine === "builtin" && !form.username?.trim() && !form.host.includes("@") && (
            <span className="wiz-hint wiz-hint--warn">{t("servers.builtinNeedsUsername")}</span>
          )}
          {form.connectionEngine === "openssh" && server?.engineNote && (
            <span className="wiz-hint">{t("servers.engineNote", { note: server.engineNote })}</span>
          )}
          {server && form.connectionEngine !== server.connectionEngine && (
            <span className="wiz-hint wiz-hint--warn">{t("servers.engineSwitchHint")}</span>
          )}
        </div>
        {form.authMethod === "password" && (
          <>
            <TextField
              label={t("servers.password")}
              type="password"
              autoComplete="new-password"
              placeholder={server?.hasPassword && !form.clearPassword ? t("servers.passwordSaved") : t("servers.passwordOptional")}
              value={form.password ?? ""}
              disabled={!!form.clearPassword}
              onChange={(e) => set({ password: e.target.value })}
              aria-invalid={!!fieldError("password") || !!fieldError("passwordUser")}
            />
            <span className="wiz-hint">{t("servers.passwordHint")}</span>
            {server?.hasPassword && (
              <label className="pl-check">
                <input type="checkbox" checked={!!form.clearPassword} onChange={(e) => set({ clearPassword: e.target.checked, password: null })} />
                {t("servers.clearPassword")}
              </label>
            )}
            {(fieldError("password") || fieldError("passwordUser")) && <span className="srv-form__err" role="alert">{fieldError("password") ?? fieldError("passwordUser")}</span>}
          </>
        )}
        {form.authMethod === "key" && (
          <>
            <TextField label={t("servers.identityFile")} className="pl-input mono" placeholder={t("servers.identityFilePlaceholder")} value={form.identityFile ?? ""} onChange={(e) => set({ identityFile: e.target.value || null })} aria-invalid={!!fieldError("identityFile")} />
            {fieldError("identityFile") && <span className="srv-form__err" role="alert">{fieldError("identityFile")}</span>}
          </>
        )}

        <div className="srv-form__row">
          <label className="pl-field">
            {t("servers.group")}
            <SuggestInput value={form.groupName ?? ""} suggestions={groups} placeholder={t("servers.groupPlaceholder")} onChange={(v) => set({ groupName: v || null })} />
          </label>
          <TextField label={t("servers.loginShell")} className="pl-input mono" placeholder={t("servers.loginShellPlaceholder")} value={form.loginShell ?? ""} onChange={(e) => set({ loginShell: e.target.value || null })} aria-invalid={!!fieldError("loginShell")} />
        </div>
        {fieldError("loginShell")
          ? <span className="srv-form__err" role="alert">{fieldError("loginShell")}</span>
          : <span className="srv-form__hint">{t("servers.loginShellHint")}</span>}


        <label className="pl-field">
          {t("servers.notes")}
          <textarea className="pl-input srv-form__notes" rows={3} value={form.notes} onChange={(e) => set({ notes: e.target.value })} />
        </label>

        {error && !error.field && <Banner tone="danger" title={t("servers.saveFailed")}>{error.text}</Banner>}
        {/* 让回车提交表单 */}
        <button type="submit" hidden />
      </form>
    </Dialog>
  );
}
