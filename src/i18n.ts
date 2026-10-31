import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import zhCN from "./locales/zh-CN.json";
import en from "./locales/en.json";

export { LANGUAGES, pickLanguage, type Language } from "./language";
import { pickLanguage } from "./language";

void i18n.use(initReactI18next).init({
  resources: { "zh-CN": { translation: zhCN }, en: { translation: en } },
  lng: pickLanguage(navigator.language),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
  returnNull: false,
});

export default i18n;
