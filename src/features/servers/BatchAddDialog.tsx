import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Badge, Banner, Button, Dialog, Segmented } from "../../components";
import { formatError } from "../../errors";
import { parseBatch, targetKey } from "./batch";
import { getServersApi, type ServerInputView, type ServerRecord } from "./api";
import "./servers.css";

type Auth = "agent" | "password";

/**
 * 批量添加服务器：粘贴多行文本，每行一台。先预览每行的解析结果，有问题的行标出原因，确认后一次加入。
 * 认证方式统一选择（ssh-agent / 默认密钥，或密码）；密码可以之后在每台服务器的设置里保存。
 */
export function BatchAddDialog({ existing, onAdded, onClose }: { existing: ServerRecord[]; onAdded: () => void; onClose: () => void }) {
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const [auth, setAuth] = useState<Auth>("agent");
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState<{ line: number; text: string; reason: string }[]>([]);

  const keys = useMemo(
    () => existing.filter((s) => s.source === "manual" && s.host).map((s) => targetKey(s.username, s.host ?? "", s.port ?? 22)),
    [existing],
  );
  const lines = useMemo(() => parseBatch(text, keys), [text, keys]);
  const valid = lines.filter((l) => l.server);

  const submit = async () => {
    setBusy(true);
    setFailed([]);
    const api = await getServersApi();
    const fails: { line: number; text: string; reason: string }[] = [];
    for (const l of valid) {
      const s = l.server!;
      const input: ServerInputView = {
        id: null, name: s.name, groupName: s.groupName, host: s.host, port: s.port, username: s.username,
        authMethod: auth, identityFile: null, loginShell: null, notes: "", connectionEngine: "builtin",
      };
      try {
        await api.save(input);
      } catch (e) {
        fails.push({ line: l.line, text: l.text, reason: formatError(e) });
      }
    }
    setBusy(false);
    onAdded();
    if (fails.length === 0) { onClose(); return; }
    // 只留下失败的行，改好后可以再点一次
    setFailed(fails);
    setText(fails.map((f) => f.text).join("\n"));
  };

  return (
    <Dialog
      title={t("batch.title")}
      size="lg"
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="primary" disabled={busy || valid.length === 0} onClick={() => void submit()}>
          {busy ? t("batch.adding") : t("batch.submit", { count: valid.length })}
        </Button>
      </>}
    >
      <div className="batch">
        <p className="imp__lead">{t("batch.lead")}</p>
        <pre className="batch__example mono">{t("batch.example")}</pre>
        <textarea
          className="pl-input batch__text mono"
          rows={8}
          autoFocus
          spellCheck={false}
          placeholder={t("batch.placeholder")}
          value={text}
          readOnly={busy}
          onChange={(e) => { setText(e.target.value); setFailed([]); }}
          aria-label={t("batch.title")}
        />
        <div className="wiz-field">
          <span className="wiz-field__label">{t("servers.auth")}</span>
          <Segmented<Auth>
            label={t("servers.auth")}
            value={auth}
            onChange={setAuth}
            options={(["agent", "password"] as Auth[]).map((v) => ({ value: v, label: t(`servers.authMethod.${v}`) }))}
          />
          <span className="wiz-hint">{auth === "password" ? t("batch.passwordHint") : t("servers.authHint.agent")}</span>
        </div>
        {lines.length > 0 && (
          <div className="imp__list" role="list" aria-label={t("batch.preview")}>
            {lines.map((l) => (
              <div key={l.line} role="listitem" className={`imp__row${l.server ? "" : " imp__row--off"}`}>
                <span className="batch__no mono">{l.line}</span>
                <span className="imp__main">
                  {l.server ? (
                    <>
                      <span className="imp__alias">{l.server.name}</span>
                      <span className="imp__muted mono">
                        {l.server.username ? `${l.server.username}@` : ""}{l.server.host}{l.server.port !== 22 ? `:${l.server.port}` : ""}
                      </span>
                    </>
                  ) : (
                    <>
                      <span className="mono">{l.text}</span>
                      <span className="batch__err">{t(`batch.error.${l.error}`)}</span>
                    </>
                  )}
                </span>
                {l.server?.groupName && <Badge>{l.server.groupName}</Badge>}
              </div>
            ))}
          </div>
        )}
        {failed.length > 0 && (
          <Banner tone="danger" title={t("batch.failed", { count: failed.length })}>
            {failed.map((f) => `${f.line}: ${f.reason}`).join("；")}
          </Banner>
        )}
      </div>
    </Dialog>
  );
}
