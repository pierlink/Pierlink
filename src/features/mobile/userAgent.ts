/**
 * 把手机浏览器的 User-Agent 转成简短说明（如“iPhone · Safari”“Android 14 · 微信”），
 * 用来在“已配对的手机”里区分同名的手机。
 */

/** 系统：iPhone / iPad / Android 版本 / 鸿蒙 / Windows / macOS / Linux */
function system(ua: string): string | null {
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  if (/HarmonyOS|OpenHarmony/i.test(ua)) return "HarmonyOS";
  const android = /Android\s+([\d.]+)/.exec(ua);
  if (android) return `Android ${android[1].split(".")[0]}`;
  if (/Android/.test(ua)) return "Android";
  if (/Windows NT/.test(ua)) return "Windows";
  if (/Mac OS X|Macintosh/.test(ua)) return "macOS";
  if (/Linux/.test(ua)) return "Linux";
  return null;
}

/** 浏览器：按“套壳的在前、通用内核在后”的顺序认 */
const BROWSERS: [RegExp, string][] = [
  [/MicroMessenger/i, "WeChat"],
  [/\bQQ\//, "QQ"],
  [/MQQBrowser/i, "QQ Browser"],
  [/UCBrowser|UCWEB/i, "UC"],
  [/Quark/i, "Quark"],
  [/MiuiBrowser|XiaoMi/i, "Mi Browser"],
  [/HuaweiBrowser/i, "Huawei Browser"],
  [/SamsungBrowser/i, "Samsung Internet"],
  [/EdgA|EdgiOS|Edg\//, "Edge"],
  [/FxiOS|Firefox\//, "Firefox"],
  [/OPR\/|OPiOS/, "Opera"],
  [/CriOS|Chrome\//, "Chrome"],
  [/Safari\//, "Safari"],
];

/**
 * @param ua 配对时记下的 User-Agent；没有时为 null
 * @returns 简短说明；什么都认不出时返回 null
 */
export function deviceSummary(ua: string | null | undefined): string | null {
  if (!ua) return null;
  const os = system(ua);
  const browser = BROWSERS.find(([re]) => re.test(ua))?.[1] ?? null;
  const parts = [os, browser].filter((x): x is string => !!x);
  return parts.length ? parts.join(" · ") : null;
}
