import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Wifi, WifiOff } from "lucide-react";
import { Notice } from "../../components";
import { inTauri } from "./TitleBar";
import "./network.css";

/** 与 Rust 端一致：系统报告的网络连通性 */
type Connectivity = "none" | "localOnly" | "internet" | "unknown";
/** 与 Rust 端 lib.rs 的 NETWORK_EVENT 一致 */
const NETWORK_EVENT = "pierlink://network";

/**
 * 本机网络断开时的全局提示（浮在窗口底部，不挡标签栏，不关窗口、不关标签）：
 * 同步和隧道在网络恢复后自动重连；终端已经断开的，按 Enter 重新连接。恢复后显示几秒“网络已恢复”。
 * 来源：Windows 网络连通性通知（Tauri 事件）；浏览器预览里用 online/offline 事件。
 */
export function NetworkBanner() {
  const { t } = useTranslation();
  const [offline, setOffline] = useState(false);
  const [restored, setRestored] = useState(false);
  const was = useRef(false);

  useEffect(() => {
    const apply = (down: boolean) => {
      if (was.current && !down) {
        setRestored(true);
        setTimeout(() => setRestored(false), 4000);
      }
      if (down) setRestored(false);
      was.current = down;
      setOffline(down);
    };
    if (!inTauri) {
      const on = () => apply(false);
      const off = () => apply(true);
      window.addEventListener("online", on);
      window.addEventListener("offline", off);
      if (!navigator.onLine) apply(true);
      return () => { window.removeEventListener("online", on); window.removeEventListener("offline", off); };
    }
    let alive = true;
    let unlisten: (() => void) | undefined;
    // 只有明确“没有网络”才提示；只有局域网（如公司内网服务器）不算断网
    // 启动时先查一次（应用启动时就没有网络的情况）
    void import("../../bindings/commands")
      .then(({ commands }) => commands.appConnectivity())
      .then((c) => { if (alive && c === "none") apply(true); })
      .catch(() => { /* 查不到时只靠变化通知 */ });
    void import("@tauri-apps/api/event")
      .then(({ listen }) => listen<Connectivity>(NETWORK_EVENT, (e) => apply(e.payload === "none")))
      .then((u) => { if (alive) unlisten = u; else u(); });
    return () => { alive = false; unlisten?.(); };
  }, []);

  if (!offline && !restored) return null;
  return (
    <Notice order={1}>
    <div className={`net-banner${offline ? "" : " net-banner--ok"}`} role={offline ? "alert" : "status"}>
      {offline ? <WifiOff size={16} aria-hidden /> : <Wifi size={16} aria-hidden />}
      <span className="net-banner__text">
        <b>{offline ? t("network.offlineTitle") : t("network.restoredTitle")}</b>
        {offline && <span>{t("network.offlineBody")}</span>}
      </span>
    </div>
    </Notice>
  );
}
