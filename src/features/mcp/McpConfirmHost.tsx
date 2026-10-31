import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Dialog } from "../../components";
import { localizeAction } from "./actionText";
import { getMcpApi, type McpAnswer, type McpConfirm } from "./api";
import "./mcp.css";

/**
 * 全局的 AI 接入确认框：agent 第一次调用工具时确认这个连接；要执行危险命令、写脚本文件时确认这一次操作。
 * 后端 60 秒没有回答按拒绝处理，并通知这里关掉对应的框。默认焦点在“拒绝”上，按 Esc 也是拒绝。
 */
export function McpConfirmHost() {
  const { t, i18n } = useTranslation();
  const [queue, setQueue] = useState<McpConfirm[]>([]);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let alive = true;
    const offs: (() => void)[] = [];
    const keep = (u: () => void) => { if (alive) offs.push(u); else u(); };
    void getMcpApi().then((api) => {
      void api.onConfirm((c) => setQueue((q) => (q.some((x) => x.id === c.id) ? q : [...q, c]))).then(keep);
      void api.onConfirmClosed((id) => setQueue((q) => q.filter((x) => x.id !== id))).then(keep);
      // 页面重新加载前已经在等的确认
      void api.pendingConfirms().then((list) => { if (alive && list.length) setQueue((q) => [...q, ...list.filter((c) => !q.some((x) => x.id === c.id))]); }, () => {});
    });
    return () => { alive = false; offs.forEach((u) => u()); };
  }, []);

  const head = queue[0];
  if (!head) return null;

  const answer = async (value: McpAnswer) => {
    setBusy(true);
    try {
      await (await getMcpApi()).answer(head.id, value);
    } catch (e) {
      console.error(e);
    } finally {
      setBusy(false);
      setQueue((q) => q.filter((x) => x.id !== head.id));
    }
  };
  const client = head.client || t("mcp.unknownClient");
  const entry = head.entry === "windows" ? t("mcp.targetWindows") : head.entry;

  return (
    <Dialog
      title={head.kind === "connect" ? t("mcp.confirmConnectTitle") : head.kind === "dangerous" ? t("mcp.confirmDangerTitle") : t("mcp.confirmActionTitle")}
      danger={head.kind === "dangerous"}
      onDismiss={busy ? undefined : () => void answer("deny")}
      actions={<>
        <Button onClick={() => void answer("deny")} disabled={busy} autoFocus>{t("mcp.deny")}</Button>
        <Button variant={head.kind === "dangerous" ? "danger" : "secondary"} onClick={() => void answer("allow")} disabled={busy}>{t("mcp.allowOnce")}</Button>
        {head.kind === "connect"
          ? <Button variant="primary" onClick={() => void answer("allow_always")} disabled={busy}>{t("mcp.allowAlways")}</Button>
          : <Button onClick={() => void answer("allow_always")} disabled={busy}>{t("mcp.allowSession")}</Button>}
      </>}
    >
      <div className="mcp-confirm">
        {head.kind === "connect" ? (
          <>
            <p>{t("mcp.confirmConnectBody", { client, entry })}</p>
            <ul className="mcp-confirm__targets">
              {head.targets.map((x) => <li key={x} className="mono">{x === "windows" ? t("mcp.targetWindows") : x}</li>)}
            </ul>
            <Banner tone="info" title={t("mcp.clientNameUntrusted")} />
          </>
        ) : (
          <>
            <p>{t("mcp.confirmDangerBody", { client, entry, target: head.target === "windows" ? t("mcp.targetWindows") : head.target ?? "" })}</p>
            <pre className="mcp-confirm__action mono">{localizeAction(head.action ?? "", i18n.language)}</pre>
            <p className="mcp-confirm__more">{t(head.entry === "windows" ? "mcp.allowSessionHint" : "mcp.allowSessionHintServer")}</p>
          </>
        )}
        {queue.length > 1 && <p className="mcp-confirm__more">{t("mcp.moreConfirms", { count: queue.length - 1 })}</p>}
      </div>
    </Dialog>
  );
}
