import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import i18n from "../i18n";
import { api, ApiError, type Me } from "./api";
import { PairScreen } from "./PairScreen";
import { OverviewTab } from "./OverviewTab";
import { TerminalsTab } from "./TerminalsTab";
import { ConfirmsTab } from "./ConfirmsTab";
import { ActivityTab } from "./ActivityTab";

type Tab = "overview" | "terminals" | "confirms" | "activity";

/** 变化通知：各页面按事件名重新拉数据（version 每次加一） */
export interface Changes { overview: number; terminals: number; confirms: number; activity: number }

/**
 * 手机页面：没配对时显示配对；配对后底部四个标签（概况、终端、确认、记录）。
 * 数据都从 Windows 上的 Pierlink 拉，变化靠事件流通知后重新拉。
 */
export function MobileApp() {
  const { t } = useTranslation();
  const [me, setMe] = useState<Me | null | "loading">("loading");
  const [tab, setTab] = useState<Tab>("overview");
  const [online, setOnline] = useState(true);
  const [changes, setChanges] = useState<Changes>({ overview: 0, terminals: 0, confirms: 0, activity: 0 });
  const [pending, setPending] = useState(0);

  const loadMe = useCallback(() => {
    // 401/403：没配对或被撤销，回到配对；网络错误时保留当前状态（首次加载除外）
    api.me().then(setMe, (e: unknown) => setMe((cur) => (e instanceof ApiError && (e.status === 401 || e.status === 403)) || cur === "loading" ? null : cur));
  }, []);
  useEffect(loadMe, [loadMe]);

  // 电脑上改权限或撤销会断开连接；重连后重新读一次自己的权限（被撤销就回到配对）
  const authed = !!me && me !== "loading";
  useEffect(() => {
    if (!authed) return;
    return api.events((name) => {
      if (name === "reconnected") loadMe();
      setChanges((c) => (name === "reconnected"
        ? { overview: c.overview + 1, terminals: c.terminals + 1, confirms: c.confirms + 1, activity: c.activity + 1 }
        : name in c ? { ...c, [name]: c[name as keyof Changes] + 1 } : c));
    }, setOnline);
  }, [authed, loadMe]);

  // 待确认数显示在标签上
  useEffect(() => {
    if (!me || me === "loading" || !me.permissions.view) return;
    api.confirms().then((l) => setPending(l.length), () => {});
  }, [me, changes.confirms]);

  if (me === "loading") return <div className="m-center">{t("common.loading")}</div>;
  if (!me) return <PairScreen onPaired={loadMe} />;

  // 被撤销或会话失效：回到配对
  const onAuthLost = () => setMe(null);
  const tabs: { id: Tab; label: string; badge?: number }[] = [
    { id: "overview", label: t("mobile.tab.overview") },
    { id: "terminals", label: t("mobile.tab.terminals") },
    { id: "confirms", label: t("mobile.tab.confirms"), badge: pending },
    { id: "activity", label: t("mobile.tab.activity") },
  ];
  return (
    <div className="m-app">
      <header className="m-head">
        <span className="m-head__title">Pierlink</span>
        <span className={`m-head__state${online ? "" : " m-head__state--off"}`}>{online ? me.name : t("mobile.offline")}</span>
      </header>
      <main className="m-main">
        {tab === "overview" && <OverviewTab me={me} version={changes.overview} onAuthLost={onAuthLost} />}
        {tab === "terminals" && <TerminalsTab me={me} version={changes.terminals} onAuthLost={onAuthLost} />}
        {tab === "confirms" && <ConfirmsTab me={me} version={changes.confirms} onAuthLost={onAuthLost} />}
        {tab === "activity" && <ActivityTab version={changes.activity} onAuthLost={onAuthLost} />}
      </main>
      <nav className="m-tabs" aria-label="Pierlink">
        {tabs.map((x) => (
          <button key={x.id} type="button" className="m-tabs__item" aria-current={tab === x.id ? "page" : undefined} onClick={() => setTab(x.id)}>
            {x.label}
            {x.badge ? <span className="m-badge">{x.badge}</span> : null}
          </button>
        ))}
      </nav>
    </div>
  );
}

/** 接口出错时：没登录 / 被撤销就回配对，其他的给出可读的说明 */
export function handleError(e: unknown, onAuthLost: () => void): string | null {
  if (e instanceof ApiError) {
    // 设备被撤销或失效：回到配对
    if (e.status === 401) { onAuthLost(); return null; }
    const known = e.code ? i18n.t(`mobile.error.${e.code.replace(/^mobile\./, "")}`, { defaultValue: "" }) : "";
    if (known) return known;
    return e.message;
  }
  // 不是入口返回的错误（fetch 失败等）：网络断了
  return i18n.t("mobile.networkError");
}
