import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Button, Dialog } from "../../components";
import { commands } from "../../bindings/commands";
import { inTauri } from "./TitleBar";

/**
 * 启动时数据库损坏、已经从每日备份恢复的话，提示一次：用的是哪天的备份、损坏的文件放在哪里。
 * 数据目录搬家被打断、需要人工恢复时也在这里提示。
 * 浏览器预览里不显示。
 */
export function DbRestoredNotice() {
  const { t } = useTranslation();
  const [info, setInfo] = useState<{ backupDate: string; corruptSavedAs: string } | null>(null);
  const [problem, setProblem] = useState(false);
  useEffect(() => {
    if (!inTauri) return;
    void commands.appDbRestored().then(setInfo, () => {});
    void commands.appDataProblem().then((e) => setProblem(!!e), () => {});
  }, []);
  // 数据目录搬家被打断（无法自动恢复）：这次不使用数据，两边的文件都保留着
  if (problem) {
    return (
      <Dialog
        title={t("dataProblem.title")}
        danger
        onDismiss={() => setProblem(false)}
        actions={<Button variant="primary" onClick={() => setProblem(false)}>{t("common.ok")}</Button>}
      >
        {t("dataProblem.body")}
      </Dialog>
    );
  }
  if (!info) return null;
  const d = info.backupDate;
  const date = /^\d{8}$/.test(d) ? `${d.slice(0, 4)}-${d.slice(4, 6)}-${d.slice(6)}` : d;
  return (
    <Dialog
      title={t("dbRestored.title")}
      onDismiss={() => setInfo(null)}
      actions={<Button variant="primary" onClick={() => setInfo(null)}>{t("common.ok")}</Button>}
    >
      <p style={{ marginTop: 0 }}>{t("dbRestored.body", { date })}</p>
      <div className="mono pl-card" style={{ fontSize: "var(--fs-xs)", overflowWrap: "anywhere" }}>{info.corruptSavedAs}</div>
    </Dialog>
  );
}
