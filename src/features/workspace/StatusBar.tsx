import { useTranslation } from "react-i18next";
import { StatusDot } from "../../components";

/** 底部状态栏：本机同步服务、隧道、冲突、日志入口。 */
export function StatusBar({ tunnelsUp, tunnelsTotal, conflicts }: { tunnelsUp: number; tunnelsTotal: number; conflicts: number }) {
  const { t } = useTranslation();
  return (
    <footer className="statusbar">
      <span style={{ display: "flex", alignItems: "center", gap: 6 }}><StatusDot tone="ok" label={t("statusbar.localSync")} />{t("statusbar.localSync")}</span>
      <span>{t("statusbar.tunnels", { up: tunnelsUp, total: tunnelsTotal })}</span>
      <span className="statusbar__spacer" />
      {conflicts > 0 && <span style={{ color: "var(--warn)" }}>{t("statusbar.conflicts", { count: conflicts })}</span>}
      <button type="button" className="pl-btn pl-btn--ghost pl-btn--sm">{t("statusbar.logs")}</button>
    </footer>
  );
}
