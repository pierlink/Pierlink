import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Dialog } from "../../components";
import { inTauri } from "../shell/TitleBar";
import { localizeAction } from "./actionText";
import { getMcpApi, type McpAnswer, type McpConfirm } from "./api";
import "./mcp.css";

/** 新的一条确认出现后，按钮多久以后才能点（毫秒） */
const ARM_MS = 500;

/**
 * 全局的 AI 接入确认框：agent 第一次调用工具时确认这个连接；要执行危险命令、写脚本文件时确认这一次操作。
 * 后端 60 秒没有回答按拒绝处理，并通知这里关掉对应的框。
 * 框是突然弹出来的，用户可能正在别处点击、打字或按 Esc：必须明确点按钮才算回答，点框外、按 Esc 都不算；
 * 焦点放在框上而不是按钮上（正在敲的回车、空格不会点到按钮）；新的一条出现后半秒内按钮不能点（连点不会答到下一条）。
 */
export function McpConfirmHost() {
  const { t, i18n } = useTranslation();
  const [queue, setQueue] = useState<McpConfirm[]>([]);
  const [busy, setBusy] = useState(false);
  // 当前这一条的按钮能不能点了（出现后等 ARM_MS）
  const [armed, setArmed] = useState<number | null>(null);
  const headId = queue[0]?.id ?? null;
  // 有确认在等时窗口标题写上（任务栏上看得到）；后端同时会让任务栏按钮闪烁
  const waiting = queue.length;
  useEffect(() => {
    if (!inTauri) return;
    const title = waiting ? `${t("mcp.waitingTitle", { count: waiting })} — Pierlink` : "Pierlink";
    void import("@tauri-apps/api/window").then(({ getCurrentWindow }) => getCurrentWindow().setTitle(title)).catch(() => undefined);
  }, [waiting, t]);
  useEffect(() => {
    if (headId === null) return;
    const timer = setTimeout(() => setArmed(headId), ARM_MS);
    return () => clearTimeout(timer);
  }, [headId]);

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
  const off = busy || armed !== head.id;
  const client = head.client || t("mcp.unknownClient");
  const entry = head.entry === "windows" ? t("mcp.targetWindows") : head.entry;

  return (
    <Dialog
      title={head.kind === "connect" ? t("mcp.confirmConnectTitle") : head.kind === "dangerous" ? t("mcp.confirmDangerTitle") : t("mcp.confirmActionTitle")}
      danger={head.kind === "dangerous"}
      actions={<>
        <Button onClick={() => void answer("deny")} disabled={off}>{t("mcp.deny")}</Button>
        <Button variant={head.kind === "dangerous" ? "danger" : "secondary"} onClick={() => void answer("allow")} disabled={off}>{t("mcp.allowOnce")}</Button>
        {head.kind === "connect"
          ? <Button variant="primary" onClick={() => void answer("allow_always")} disabled={off}>{t("mcp.allowAlways")}</Button>
          : <Button onClick={() => void answer("allow_always")} disabled={off}>{t("mcp.allowSession")}</Button>}
      </>}
    >
      <div className="mcp-confirm" tabIndex={-1}>
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
