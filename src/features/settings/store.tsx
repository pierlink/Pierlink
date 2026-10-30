import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import i18n, { pickLanguage } from "../../i18n";
import { applyTheme } from "../../theme";
import { normalizeSettings, type AppSettings } from "./model";

/**
 * 设置的存储位置。目前保存在本机浏览器存储（WebView2 的用户数据目录里，随应用卸载清除）；
 * 后端设置接口接入后改为写数据库的 settings 表，界面不需要改。
 */
const KEY = "pierlink.settings.v1";

function load(): AppSettings {
  try {
    const text = window.localStorage.getItem(KEY);
    return normalizeSettings(text ? JSON.parse(text) : undefined);
  } catch {
    // 存储不可用或内容损坏：用默认设置
    return normalizeSettings(undefined);
  }
}

function save(s: AppSettings) {
  try {
    window.localStorage.setItem(KEY, JSON.stringify(s));
  } catch {
    // 存储不可用时只在本次运行中生效
  }
}

/** 把主题和语言应用到界面上 */
export function applySettings(s: AppSettings) {
  applyTheme(s.theme);
  const lang = s.language === "system" ? pickLanguage(navigator.language) : s.language;
  if (i18n.language !== lang) void i18n.changeLanguage(lang);
  document.documentElement.lang = lang;
}

interface Ctx {
  settings: AppSettings;
  /** 修改设置：传入返回新设置的函数，结果会被合法化后保存并立即生效 */
  update: (fn: (s: AppSettings) => AppSettings) => void;
}

const SettingsContext = createContext<Ctx | null>(null);

export function SettingsProvider({ children }: { children: ReactNode }) {
  // 首次读取时立即应用主题，避免页面先以默认主题闪一下
  const [settings, setSettings] = useState<AppSettings>(() => {
    const s = load();
    applySettings(s);
    return s;
  });

  useEffect(() => { applySettings(settings); }, [settings]);

  // 跟随系统主题时，系统切换深浅色要立即响应
  useEffect(() => {
    if (settings.theme !== "system") return;
    const mq = window.matchMedia?.("(prefers-color-scheme: dark)");
    const onChange = () => applyTheme("system");
    mq?.addEventListener("change", onChange);
    return () => mq?.removeEventListener("change", onChange);
  }, [settings.theme]);

  const update = useCallback((fn: (s: AppSettings) => AppSettings) => {
    setSettings((prev) => {
      const next = normalizeSettings(fn(prev));
      save(next);
      return next;
    });
  }, []);

  const value = useMemo(() => ({ settings, update }), [settings, update]);
  return <SettingsContext.Provider value={value}>{children}</SettingsContext.Provider>;
}

/** 读取和修改设置 */
export function useSettings(): Ctx {
  const ctx = useContext(SettingsContext);
  if (!ctx) throw new Error("useSettings 必须在 SettingsProvider 内使用");
  return ctx;
}
