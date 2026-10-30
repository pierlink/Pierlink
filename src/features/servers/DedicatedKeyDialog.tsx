import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Badge, Banner, Button, Dialog, TextField } from "../../components";
import { formatError, isAppError } from "../../errors";
import { getKeyApi, type KeyInstallResult, type KeyStatus } from "./keyApi";
import type { ServerRecord } from "./api";
import "./servers.css";

/**
 * 专用密钥（设计 3.1）：本电脑一把 Pierlink 专用密钥，所有服务器共用。
 * - 没装时：输一次服务器密码安装，装完立即只用密钥登录验证，失败自动撤销；
 * - 装好后：列出服务器上与 Pierlink 有关的密钥（本电脑的、其他电脑或旧的），可逐行移除。
 * 密码只交给后端用一次，不保存；登录失败一次就停，不自动重试（S18）。
 */
export function DedicatedKeyDialog({ server, onChanged, onClose }: { server: ServerRecord; onChanged: () => void; onClose: () => void }) {
  const { t } = useTranslation();
  // 对话框里自己记住当前状态：安装、移除后父组件传入的服务器记录不会马上更新
  const [installedHere, setInstalledHere] = useState(server.authMethod === "dedicated_key");
  const [removedOurs, setRemovedOurs] = useState(false);
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<KeyInstallResult | null>(null);
  const [status, setStatus] = useState<KeyStatus | null>(null);
  const [statusError, setStatusError] = useState<{ text: string; reinstall: boolean } | null>(null);
  const [confirm, setConfirm] = useState<string | null>(null);

  /** 错误说明：服务器上的锁被占用时说清楚原因 */
  const describe = useCallback((e: unknown) => {
    if (isAppError(e) && e.params.exit_code === "71") return t("keys.lockBusy");
    return formatError(e);
  }, [t]);

  const loadStatus = useCallback(() => {
    setStatusError(null);
    void getKeyApi().then((api) => api.status(server.id)).then(setStatus, (e: unknown) => {
      // 本机密钥丢失或损坏（口令条目不见了等）、或服务器上那一行没了：直接给出重新安装的入口
      const code = isAppError(e) ? e.code : "";
      setStatusError({ text: describe(e), reinstall: ["keys.local_missing", "keys.local_failed", "ssh.auth_failed"].includes(code) });
    });
  }, [server.id, describe]);
  // 只有已经用专用密钥登录的服务器才能在后台读取 authorized_keys
  useEffect(() => { if (installedHere) loadStatus(); }, [installedHere, loadStatus]);

  const install = async () => {
    setBusy(true);
    setError(null);
    setDone(null);
    setRemovedOurs(false);
    try {
      const r = await (await getKeyApi()).install(server.id, password);
      setDone(r);
      setPassword("");
      setStatusError(null);
      setInstalledHere(true);
      onChanged();
    } catch (e) {
      // 密码错误只提示，不自动重试，避免触发服务器的登录保护
      // 只有 ssh.auth_failed 才是密码不对；keys.verify_failed（密钥登录被拒、已撤销）有自己的说明
      if (isAppError(e) && e.code === "ssh.auth_failed") setError(t("keys.wrongPassword"));
      else if (isAppError(e) && e.code === "keys.verify_failed" && e.params.fingerprint) {
        setError(`${formatError(e)} ${t("keys.removeManually", { fingerprint: e.params.fingerprint, comment: e.params.comment ?? "" })}`);
      } else setError(describe(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (publicLine: string, ours: boolean) => {
    setBusy(true);
    setError(null);
    // 上一次“安装成功”的提示不再适用
    setDone(null);
    try {
      const r = await (await getKeyApi()).remove(server.id, publicLine);
      setConfirm(null);
      if (r === "symlink") { setError(t("keys.symlink")); return; }
      if (ours) {
        // 本电脑的密钥移除后改回密码登录，再也不能用密钥读取列表：直接显示安装入口
        setInstalledHere(false);
        setStatus(null);
        setRemovedOurs(true);
        onChanged();
        return;
      }
      loadStatus();
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(false);
    }
  };

  const showInstall = !installedHere || !!statusError?.reinstall;

  return (
    <Dialog
      title={t("keys.title", { name: server.name })}
      size="lg"
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{showInstall ? t("common.cancel") : t("deploy.close")}</Button>
        {showInstall && (
          <Button variant="primary" disabled={busy || !password} onClick={() => void install()}>
            {busy ? t("keys.installing") : t("keys.install")}
          </Button>
        )}
      </>}
    >
      <div className="keys">
        <p className="imp__lead">{t("keys.lead")}</p>
        {removedOurs && <Banner tone="ok" title={t("keys.removedOursTitle")}>{t("keys.removedOursBody")}</Banner>}
        {done && <Banner tone="ok" title={done.outcome === "installed" ? t("keys.installed") : t("keys.alreadyPresent")}>{t("keys.nowUsingKey")}</Banner>}
        {done?.permissionsWarning && <Banner tone="warn" title={t("keys.warning")}>{t("keys.permissionsWarning")}</Banner>}
        {done?.nonDefaultAuthorizedKeys && <Banner tone="info" title={t("keys.warning")}>{t("keys.nonDefault")}</Banner>}
        {done?.regenerated && done.affectedServers.length > 0 && (
          <Banner tone="warn" title={t("keys.regeneratedTitle")}>{t("keys.regeneratedBody", { servers: done.affectedServers.join(t("common.listSep")) })}</Banner>
        )}
        {showInstall && statusError?.reinstall && <Banner tone="warn" title={t("keys.reinstallTitle")}>{statusError.text}</Banner>}
        {showInstall && (
          <form className="keys__form" onSubmit={(e) => { e.preventDefault(); if (password && !busy) void install(); }}>
            <ol className="keys__steps">
              <li>{t("keys.step1")}</li>
              <li>{t("keys.step2")}</li>
              <li>{t("keys.step3")}</li>
            </ol>
            <TextField
              label={t("keys.password", { target: server.username && server.host ? `${server.username}@${server.host}` : server.name })}
              type="password"
              autoComplete="off"
              autoFocus
              value={password}
              readOnly={busy}
              onChange={(e) => { setPassword(e.target.value); setError(null); }}
            />
            <span className="imp__muted keys__note">{t("keys.passwordNote")}</span>
          </form>
        )}
        {error && <Banner tone="danger" title={t("keys.failed")}>{error}</Banner>}

        {!showInstall && (
          <div className="keys__list">
            <div className="keys__head">
              <span>{t("keys.onServer")}</span>
              <Button size="sm" variant="ghost" onClick={loadStatus} disabled={busy}>{t("common.refresh")}</Button>
            </div>
            {statusError && <Banner tone="warn" title={t("keys.statusFailed")}>{statusError.text}</Banner>}
            {!status && !statusError && <span role="status" className="imp__muted">{t("common.loading")}</span>}
            {status?.warnings.map((w, i) => <Banner key={i} tone="warn" title={t("keys.warning")}>{formatError(w)}</Banner>)}
            {status && status.keys.length === 0 && <span className="imp__muted">{t("keys.none")}</span>}
            {status && status.keys.length > 0 && (
              <ul className="item-list__rows">
                {status.keys.map((k) => (
                  <li key={`${k.lineNo}-${k.fingerprint}`} className="item-list__row">
                    <div className="item-list__main">
                      <span className="mono item-list__name" title={k.fingerprint}>{k.fingerprint}</span>
                      <span className="item-list__meta">{k.keyType} · {k.comment || t("keys.noComment")}</span>
                    </div>
                    <div className="item-list__tags">
                      {k.ours ? <Badge tone="accent">{t("keys.thisComputer")}</Badge> : <Badge>{t("keys.otherOrOld")}</Badge>}
                    </div>
                    {confirm === k.fingerprint ? (
                      <div className="item-list__actions">
                        <span className="item-list__confirm">{k.ours ? t("keys.removeOursConfirm") : t("keys.removeConfirm")}</span>
                        <Button size="sm" onClick={() => setConfirm(null)} disabled={busy}>{t("common.cancel")}</Button>
                        <Button size="sm" variant="danger" onClick={() => void remove(k.publicLine, k.ours)} disabled={busy}>{t("keys.remove")}</Button>
                      </div>
                    ) : (
                      <div className="item-list__actions">
                        <Button size="sm" variant="danger-ghost" disabled={busy} onClick={() => setConfirm(k.fingerprint)}>{t("keys.remove")}</Button>
                      </div>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </div>
    </Dialog>
  );
}
