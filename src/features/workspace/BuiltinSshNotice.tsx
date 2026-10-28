import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Button } from "../../components";
import { inTauri } from "../shell/TitleBar";
import { useSettings } from "../settings/store";
import { getServersApi } from "../servers/api";

/**
 * 升级到 0.4.0 后第一次打开时，在服务器列表顶部说明一次：应用内添加的服务器已改用内置 SSH，
 * 从 ssh config 导入的保持原样，连不上时可以在服务器设置里改回系统 OpenSSH。
 * 放在列表里（不弹窗、不盖住界面）；没有这类服务器就直接记为已说明。
 */
export function BuiltinSshNotice() {
  const { t } = useTranslation();
  const { settings, update } = useSettings();
  const [count, setCount] = useState(0);
  const shown = settings.notices.builtinSsh;
  const dismiss = () => update((s) => ({ ...s, notices: { ...s.notices, builtinSsh: true } }));

  useEffect(() => {
    if (!inTauri || shown) return;
    let alive = true;
    void getServersApi()
      .then((api) => api.list())
      .then((list) => {
        if (!alive) return;
        const n = list.filter((s) => s.source === "manual" && s.connectionEngine === "builtin").length;
        if (n === 0) dismiss();
        else setCount(n);
      }, () => { /* 读不到列表时下次再说 */ });
    return () => { alive = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shown]);

  if (shown || count === 0) return null;
  return (
    <div className="srv-notice" role="note">
      <b>{t("notices.builtinSsh.title")}</b>
      <span>{t("notices.builtinSsh.body", { count })}</span>
      <Button size="sm" onClick={dismiss}>{t("notices.builtinSsh.ok")}</Button>
    </div>
  );
}
