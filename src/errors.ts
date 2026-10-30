import i18n from "./i18n";

/** 后端返回的错误：错误码 + 可展示的参数（与 Rust 的 AppError 一致） */
export interface AppError {
  code: string;
  params: Record<string, string>;
}

/** 判断一个值是否是后端返回的 AppError */
export function isAppError(v: unknown): v is AppError {
  return !!v && typeof v === "object" && typeof (v as AppError).code === "string" && typeof (v as AppError).params === "object";
}

/**
 * 把后端错误翻译成当前语言的文字；没有对应翻译时显示通用提示和错误码。
 * @param err 任意错误值
 * @returns 给用户看的一句话
 */
export function formatError(err: unknown): string {
  if (!isAppError(err)) return i18n.t("errors.unknown", { code: "frontend" });
  const key = `errors.${err.code}`;
  return i18n.exists(key) ? i18n.t(key, translatedParams(err.params)) : i18n.t("errors.unknown", { code: err.code });
}

/**
 * 后端的固定说明带键：参数 X 旁边有 X_key 时，按当前语言显示 errors.detail.<键> 的译文（译文里可以引用其他参数），
 * 代替后端写的中文原文。没有对应译文时照旧显示原文。
 * @param params 后端错误的参数
 * @returns 用于翻译的参数
 */
function translatedParams(params: Record<string, string>): Record<string, string> {
  const out = { ...params };
  for (const [name, value] of Object.entries(params)) {
    if (!name.endsWith("_key") || !value) continue;
    const key = `errors.detail.${value}`;
    if (i18n.exists(key)) out[name.slice(0, -"_key".length)] = i18n.t(key, params);
  }
  return out;
}
