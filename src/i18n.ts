import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import zhCN from "./locales/zh-CN.json";
import en from "./locales/en.json";

/** 支持的界面语言 */
export const LANGUAGES = ["zh-CN", "en"] as const;
export type Language = (typeof LANGUAGES)[number];

/**
 * 根据系统语言选出界面语言：中文系统用简体中文，其余用英文。
 * @param systemLang 浏览器/系统报告的语言代码，如 "zh-CN"、"en-US"
 * @returns 支持的语言代码
 */
export function pickLanguage(systemLang: string | undefined): Language {
  return systemLang?.toLowerCase().startsWith("zh") ? "zh-CN" : "en";
}

void i18n.use(initReactI18next).init({
  resources: { "zh-CN": { translation: zhCN }, en: { translation: en } },
  lng: pickLanguage(navigator.language),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
  returnNull: false,
});

export default i18n;
