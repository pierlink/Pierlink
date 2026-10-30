import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Badge, Banner, Button, Dialog, SuggestInput } from "../../components";
import { formatError, isAppError } from "../../errors";
import { getServersApi, type ConversionPreview, type ResolvedView, type ServerRecord } from "./api";
import "./servers.css";

/**
 * 导入自 ssh config 的服务器：连接参数只读显示（每次打开时用 `ssh -G` 重新解析），
 * 只能修改应用自己的设置（分组、备注）。别名已从配置里删掉时如实提示。
 * 可以转换成内置 SSH：先检查（预览会抄进去的字段和不能转的原因），确认后转换成应用内添加的服务器。
 */
export function LinkServerDialog({ server, groups, onSaved, onClose }: {
  server: ServerRecord;
  groups: string[];
  onSaved: () => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [resolved, setResolved] = useState<ResolvedView | null>(null);
  const [resolveError, setResolveError] = useState<string | null>(null);
  const [group, setGroup] = useState(server.groupName ?? "");
  const [notes, setNotes] = useState(server.notes);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [conv, setConv] = useState<{ kind: "idle" } | { kind: "busy" } | { kind: "preview"; preview: ConversionPreview } | { kind: "error"; text: string }>({ kind: "idle" });

  /** 检查能否转换成内置 SSH（会用 ssh -G 解析这台和各级跳板，不改数据） */
  const checkConversion = () => {
    setConv({ kind: "busy" });
    getServersApi().then((api) => api.convertPreview(server.id)).then(
      (preview) => setConv({ kind: "preview", preview }),
      (e: unknown) => setConv({ kind: "error", text: formatError(e) }),
    );
  };

  /** 按刚才的预览转换；ssh config 在这期间变了就重新检查 */
  const convert = async () => {
    setBusy(true);
    setError(null);
    try {
      await (await getServersApi()).convert(server.id);
      onSaved();
      onClose();
    } catch (e) {
      if (isAppError(e) && e.code === "ssh.conversion_changed") checkConversion();
      setError(formatError(e));
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    let alive = true;
    void getServersApi().then((api) => api.resolve(server.id)).then(
      (r) => { if (alive) setResolved(r); },
      (e: unknown) => { if (alive) setResolveError(formatError(e)); },
    );
    return () => { alive = false; };
  }, [server.id]);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await (await getServersApi()).updateLink(server.id, group.trim() || null, notes);
      onSaved();
      onClose();
    } catch (e) {
      setError(formatError(e));
    } finally {
      setBusy(false);
    }
  };

  const row = (label: string, value: string) => (
    <div className="link-info__row"><span className="link-info__key">{label}</span><span className="mono link-info__val">{value}</span></div>
  );

  return (
    <Dialog
      title={t("servers.editTitle", { name: server.name })}
      size="lg"
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="primary" disabled={busy} onClick={() => void save()}>{t("common.save")}</Button>
      </>}
    >
      <form className="srv-form" onSubmit={(e) => { e.preventDefault(); void save(); }}>
        <p className="imp__lead">{t("servers.linkLead", { alias: server.sshAlias ?? server.name })}</p>
        <div className="link-info" aria-busy={!resolved && !resolveError}>
          {!resolved && !resolveError && <span role="status" className="imp__muted">{t("servers.resolving")}</span>}
          {resolveError && <Banner tone="warn" title={t("servers.resolveFailed")}>{resolveError}</Banner>}
          {resolved && (
            <>
              {row(t("servers.host"), resolved.hostname)}
              {row(t("servers.username"), resolved.user)}
              {row(t("servers.port"), String(resolved.port))}
              {resolved.identityFiles.length > 0 && row(t("servers.identityFile"), resolved.identityFiles.join("\n"))}
              {resolved.proxyJump && row(t("servers.jump"), resolved.proxyJump)}
              {(resolved.viaProxyCommand || resolved.hasForwards || resolved.hostKeyCheckWeakened) && (
                <div className="link-info__tags">
                  {resolved.viaProxyCommand && <Badge tone="accent">{t("servers.viaProxyCommand")}</Badge>}
                  {resolved.hasForwards && <span title={t("import.forwardsHint")}><Badge tone="warn">{t("import.forwards")}</Badge></span>}
                  {resolved.hostKeyCheckWeakened && <span title={t("import.weakHint")}><Badge tone="danger">{t("import.weak")}</Badge></span>}
                </div>
              )}
            </>
          )}
        </div>

        <div className="link-convert">
          <div className="link-convert__head">
            <b>{t("convert.title")}</b>
            {conv.kind !== "preview" && (
              <Button size="sm" disabled={conv.kind === "busy" || busy} onClick={checkConversion}>
                {conv.kind === "busy" ? t("convert.checking") : t("convert.check")}
              </Button>
            )}
          </div>
          <span className="wiz-hint">{t("convert.lead")}</span>
          {conv.kind === "error" && <Banner tone="warn" title={t("convert.failed")}>{conv.text}</Banner>}
          {conv.kind === "preview" && conv.preview.reasons.length > 0 && (
            <Banner tone="warn" title={t("convert.blocked")}>
              <ul className="link-convert__reasons">
                {conv.preview.reasons.map((r, i) => <li key={i}>{formatError({ code: r.code, params: { option: r.option, hop: r.hop } })}</li>)}
              </ul>
              {t("convert.blockedHint")}
            </Banner>
          )}
          {conv.kind === "preview" && conv.preview.reasons.length === 0 && (
            <>
              <div className="link-info">
                {row(t("servers.host"), `${conv.preview.target.host}:${conv.preview.target.port}`)}
                {row(t("servers.username"), conv.preview.target.username)}
                {conv.preview.target.identityFiles.length > 0 && row(t("servers.identityFile"), conv.preview.target.identityFiles.join("\n"))}
                {conv.preview.target.hostKeyAlias && row("HostKeyAlias", conv.preview.target.hostKeyAlias)}
                {conv.preview.jumps.length > 0 && row(t("servers.jump"), conv.preview.jumps.map((j) => `${j.fields.alias}${j.existingServerId ? "" : t("convert.newJump")}`).join(" → "))}
              </div>
              <div className="link-convert__actions">
                <Button variant="primary" size="sm" disabled={busy} onClick={() => void convert()}>{t("convert.do")}</Button>
              </div>
            </>
          )}
        </div>

        <label className="pl-field">
          {t("servers.group")}
          <SuggestInput value={group} suggestions={groups} placeholder={t("servers.groupPlaceholder")} onChange={setGroup} />
        </label>
        <label className="pl-field">
          {t("servers.notes")}
          <textarea className="pl-input srv-form__notes" rows={3} value={notes} onChange={(e) => setNotes(e.target.value)} />
        </label>
        {error && <Banner tone="danger" title={t("servers.saveFailed")}>{error}</Banner>}
        <button type="submit" hidden />
      </form>
    </Dialog>
  );
}
