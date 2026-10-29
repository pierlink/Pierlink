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
  return i18n.exists(key) ? i18n.t(key, withDetail(err.params)) : i18n.t("errors.unknown", { code: err.code });
}

/**
 * 内置 SSH 的错误带 detail（固定说明的键）：按当前语言翻译后填进 stderr_tail、reason 占位符，代替后端的中文兜底。
 * @param params 后端错误的参数
 * @returns 用于翻译的参数；没有 detail 或没有对应翻译时原样返回
 */
function withDetail(params: Record<string, string>): Record<string, string> {
  const detail = params.detail;
  const key = `errors.detail.${detail}`;
  if (!detail || !i18n.exists(key)) return params;
  const text = i18n.t(key);
  return { ...params, stderr_tail: text, reason: text };
}
