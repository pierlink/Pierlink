import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Checkbox, Dialog, TextField } from "../../components";
import { formatError } from "../../errors";
import type { ServerRecord } from "../servers/api";
import { getPasswordsApi, type PasswordRequest } from "./api";
import { passwordBus } from "./bus";
import { canSharePassword } from "./share";
import "./passwords.css";

/** 当前显示的框：后端推来的请求，或用户点“输入密码”打开的 */
type Current = { from: "backend"; req: PasswordRequest } | { from: "user"; serverId: string; serverName: string; canRemember: boolean };

/**
 * 全局密码框：ssh 要密码时（终端、文件面板）由后端推来请求；后台功能“等待输入密码”时由用户点按钮打开。
 * 服务器密码输一次，本次运行里终端、资源概览、同步、端口转发、文件面板一起用；可以顺手记住。
 * @param servers 服务器列表（取名字、判断能否记住密码）
 * @param onDedicatedKey 用户选择“改为安装专用密钥”
 */
export function PasswordHost({ servers, onDedicatedKey }: { servers: ServerRecord[]; onDedicatedKey: (serverId: string) => void }) {
  const { t } = useTranslation();
  const [queue, setQueue] = useState<PasswordRequest[]>([]);
  const [entry, setEntry] = useState<string | null>(null);
  const [value, setValue] = useState("");
  const [remember, setRemember] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const serversRef = useRef(servers);
  serversRef.current = servers;

  // 后端请求、请求结束、密码已保存
  useEffect(() => {
    let alive = true;
    const offs: (() => void)[] = [];
    const keep = (u: () => void) => { if (alive) offs.push(u); else u(); };
    void getPasswordsApi().then((api) => {
      void api.onRequest((r) => setQueue((q) => (q.some((x) => x.id === r.id) ? q : [...q, r]))).then(keep);
      void api.onClosed((id) => setQueue((q) => q.filter((x) => x.id !== id))).then(keep);
      void api.onSaved((serverId) => passwordBus.emitSaved(serverId)).then(keep);
      // 页面重新加载前已经在等的请求
      void api.pending().then((list) => { if (alive && list.length) setQueue((q) => [...q, ...list.filter((r) => !q.some((x) => x.id === r.id))]); }, () => {});
    });
    const offEntry = passwordBus.onEntry((serverId) => setEntry(serverId));
    return () => { alive = false; offEntry(); offs.forEach((u) => u()); };
  }, []);

  const head = queue[0];
  const entryServer = entry ? serversRef.current.find((s) => s.id === entry) : undefined;
  // 同一台服务器已有后端在等的密码请求时，直接显示那个请求
  const current: Current | null = head
    ? { from: "backend", req: head }
    : entry
      ? { from: "user", serverId: entry, serverName: entryServer?.name ?? entry, canRemember: canSharePassword(entryServer) }
      : null;
  const currentKey = current ? (current.from === "backend" ? `b${current.req.id}` : `u${current.serverId}`) : "";

  // 换了一个框：清空输入，“记住密码”默认勾上（能记住时）
  useEffect(() => {
    setValue("");
    setError(null);
    setBusy(false);
    setRemember(true);
  }, [currentKey]);

  if (!current) return null;
  const kind = current.from === "backend" ? current.req.kind : "password";
  const serverId = current.from === "backend" ? current.req.serverId : current.serverId;
  const serverName = current.from === "backend" ? current.req.serverName : current.serverName;
  const canRemember = current.from === "backend" ? current.req.canRemember : current.canRemember;
  const waiting = queue.length - (current.from === "backend" ? 1 : 0);

  const finish = () => {
    if (current.from === "backend") {
      const id = current.req.id;
      setQueue((q) => q.filter((x) => x.id !== id));
      // 用户在这个框里答完了：同一台服务器的“输入密码”请求一并结束
      if (entry === current.req.serverId && kind === "password") setEntry(null);
    } else setEntry(null);
  };
  const cancel = () => {
    if (current.from === "backend") void getPasswordsApi().then((api) => api.answer(current.req.id, null, false)).catch(() => {});
    finish();
  };
  const submit = (answer: string | null) => {
    setBusy(true);
    setError(null);
    void getPasswordsApi().then(async (api) => {
      if (current.from === "backend") {
        const ok = await api.answer(current.req.id, answer, kind === "password" && canRemember && remember);
        // 请求已经结束（ssh 等不及先退出了）：密码照样存进内存，等待中的功能和终端会用它
        if (!ok && kind === "password" && answer) await api.enter(serverId, answer, canRemember && remember);
      } else if (answer) {
        await api.enter(serverId, answer, canRemember && remember);
      }
    }).then(finish, (e: unknown) => { setBusy(false); setError(formatError(e)); });
  };

  const valid = kind !== "password" || value.length > 0;
  const title = kind === "password" ? t("passwords.title", { name: serverName })
    : kind === "confirm" ? t("passwords.confirmTitle", { name: serverName })
    : kind === "notice" ? t("passwords.noticeTitle", { name: serverName })
    : t("passwords.otherTitle", { name: serverName });
  const actions = kind === "confirm" ? (
    <>
      <Button onClick={() => submit(null)} disabled={busy}>{t("passwords.no")}</Button>
      <Button variant="primary" onClick={() => submit("yes")} disabled={busy}>{t("passwords.yes")}</Button>
    </>
  ) : kind === "notice" ? (
    <Button variant="primary" onClick={() => submit("")} disabled={busy}>{t("passwords.gotIt")}</Button>
  ) : (
    <>
      <Button onClick={cancel} disabled={busy}>{t("common.cancel")}</Button>
      <Button variant="primary" type="submit" form="pw-form" disabled={busy || !valid}>{kind === "password" ? t("passwords.submit") : t("common.ok")}</Button>
    </>
  );

  return (
    <Dialog title={title} onDismiss={busy ? undefined : cancel} actions={actions}>
      <form
        id="pw-form"
        className="pw-form"
        onSubmit={(e) => { e.preventDefault(); if (valid && !busy) submit(value); }}
      >
        {kind === "password" ? (
          <p className="pw-form__lead">{t("passwords.lead")}</p>
        ) : (
          <>
            {/* 提示原文来自 ssh 或服务器：只当文字显示 */}
            <pre className="pw-form__prompt">{current.from === "backend" ? current.req.prompt : ""}</pre>
            {kind === "other" && <p className="pw-form__lead">{t("passwords.otherLead")}</p>}
          </>
        )}
        {(kind === "password" || kind === "other") && (
          <TextField
            label={kind === "password" ? t("passwords.field") : t("passwords.answer")}
            type="password"
            autoComplete="off"
            value={value}
            maxLength={1024}
            onChange={(e) => setValue(e.target.value)}
            autoFocus
          />
        )}
        {kind === "password" && canRemember && (
          <Checkbox label={t("passwords.remember")} checked={remember} onChange={(e) => setRemember(e.target.checked)} />
        )}
        {kind === "password" && canRemember && (
          <button
            type="button"
            className="pw-form__link"
            disabled={busy}
            onClick={() => { cancel(); onDedicatedKey(serverId); }}
          >
            {t("passwords.useKey")}
          </button>
        )}
        {waiting > 0 && <p className="pw-form__hint">{t("passwords.queued", { count: waiting })}</p>}
        {error && <Banner tone="danger" title={t("passwords.failed")}>{error}</Banner>}
      </form>
    </Dialog>
  );
}
