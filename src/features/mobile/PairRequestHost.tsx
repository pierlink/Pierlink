import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Button, Dialog } from "../../components";
import { getMobileApi, type PairRequest } from "./api";

/**
 * 手机配对请求：手机输对邀请码后，电脑上弹出“批准这台手机？”（2 分钟内不回答按拒绝）。
 * 放在应用最外层，不管当前在哪个页面都能弹出来。
 */
export function PairRequestHost() {
  const { t } = useTranslation();
  const [queue, setQueue] = useState<PairRequest[]>([]);

  useEffect(() => {
    let alive = true;
    const offs: (() => void)[] = [];
    void getMobileApi().then(async (api) => {
      const a = await api.onPairRequest((r) => setQueue((q) => [...q.filter((x) => x.id !== r.id), r]));
      const b = await api.onPairClosed((id) => setQueue((q) => q.filter((x) => x.id !== id)));
      if (alive) offs.push(a, b); else { a(); b(); }
    });
    return () => { alive = false; offs.forEach((f) => f()); };
  }, []);

  const head = queue[0];
  if (!head) return null;
  const answer = (allow: boolean) => {
    setQueue((q) => q.filter((x) => x.id !== head.id));
    void getMobileApi().then((api) => api.answerPair(head.id, allow)).catch(() => {});
  };
  return (
    <Dialog title={t("mobileSettings.pairTitle")} onDismiss={() => answer(false)} actions={<>
      <Button onClick={() => answer(false)} autoFocus>{t("mcp.deny")}</Button>
      <Button variant="primary" onClick={() => answer(true)}>{t("mobileSettings.approve")}</Button>
    </>}>
      <p>{t("mobileSettings.pairBody", { name: head.name, ip: head.ip })}</p>
      <p className="set-row__hint">{t("mobileSettings.pairHintDialog")}</p>
    </Dialog>
  );
}
