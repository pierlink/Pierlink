import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, type ActivityItem } from "./api";
import { handleError } from "./App";

function clock(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** AI 操作记录（最近 50 条） */
export function ActivityTab({ version, onAuthLost }: { version: number; onAuthLost: () => void }) {
  const { t } = useTranslation();
  const [list, setList] = useState<ActivityItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    api.activity(50).then((l) => { if (alive) { setList(l); setError(null); } }, (e: unknown) => { if (alive) setError(handleError(e, onAuthLost)); });
    return () => { alive = false; };
  }, [version, onAuthLost]);
  return (
    <div className="m-page">
      {error && <p className="m-error">{error}</p>}
      {list === null && !error && <p className="m-muted">{t("common.loading")}</p>}
      {list?.length === 0 && <p className="m-muted">{t("mcp.noActivity", { defaultValue: "—" })}</p>}
      {list?.map((a, i) => (
        <div key={`${a.timeMs}-${i}`} className="m-act">
          <span className="m-act__time">{clock(a.timeMs)}</span>
          <span className="m-act__main">{a.tool}{a.target ? ` · ${a.target}` : ""}</span>
          <span className={`m-act__out${a.outcome === "ok" ? "" : " m-act__out--bad"}`}>{a.outcome}</span>
          <span className="m-act__sum">{a.summary}</span>
        </div>
      ))}
    </div>
  );
}
