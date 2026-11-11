import { useCallback, useRef, useSyncExternalStore } from "react";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { Banner, IconButton, Notice } from "../../components";

/** 对话框关掉以后才失败的后台操作：错误放在窗口底部的提示里，点 × 关掉 */
interface LateError {
  key: number;
  title: string;
  text: string;
}

/** 最多同时显示几条（更早的丢掉） */
const KEEP = 5;

let items: LateError[] = [];
let seq = 0;
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((l) => l());
const subscribe = (l: () => void) => { listeners.add(l); return () => listeners.delete(l); };

/**
 * 报告一个对话框已经关掉后才出的错（显示在窗口底部，点 × 关掉）。
 * @param title 哪个操作失败了
 * @param text 原因
 */
export function reportLateError(title: string, text: string): void {
  items = [...items, { key: ++seq, title, text }].slice(-KEEP);
  emit();
}

/** 关掉一条提示 */
function dismiss(key: number): void {
  items = items.filter((x) => x.key !== key);
  emit();
}

/** 提示区：挂在应用外壳里，有错误时才显示 */
export function LateErrorHost() {
  const { t } = useTranslation();
  const list = useSyncExternalStore(subscribe, () => items);
  return (
    <>
      {list.map((x) => (
        <Notice key={x.key} order={5}>
          <Banner tone="danger" title={x.title} actions={<IconButton label={t("common.close")} onClick={() => dismiss(x.key)}><X size={14} /></IconButton>}>{x.text}</Banner>
        </Notice>
      ))}
    </>
  );
}

/**
 * 对话框里的操作进行中也能关掉：关掉以后操作照常做完，之后出的错交给 reportLateError，成功不再打扰。
 * @param onClose 关掉对话框
 * @returns closed：对话框已经关掉了没有（操作结束时据此决定错误显示在哪里、要不要再动对话框）；close：关掉对话框
 */
export function useDetachableClose(onClose: () => void): { closed: { readonly current: boolean }; close: () => void } {
  const closed = useRef(false);
  const close = useCallback(() => {
    closed.current = true;
    onClose();
  }, [onClose]);
  return { closed, close };
}
