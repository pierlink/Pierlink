import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import { pickLanguage } from "../language";
// 只取手机页面用到的几组（按名字导入，其余的不会打进手机页面，文件小很多）
import { common as zhCommon, mcp as zhMcp, mobile as zhMobile, monitor as zhMonitor, sync as zhSync } from "../locales/zh-CN.json";
import { common as enCommon, mcp as enMcp, mobile as enMobile, monitor as enMonitor, sync as enSync } from "../locales/en.json";

void i18n.use(initReactI18next).init({
  resources: {
    "zh-CN": { translation: { common: zhCommon, mcp: zhMcp, mobile: zhMobile, monitor: zhMonitor, sync: zhSync } },
    en: { translation: { common: enCommon, mcp: enMcp, mobile: enMobile, monitor: enMonitor, sync: enSync } },
  },
  lng: pickLanguage(navigator.language),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
  returnNull: false,
});

export default i18n;
