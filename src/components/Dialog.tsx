import { useEffect, useId, useRef, type ReactNode } from "react";
import { createPortal } from "react-dom";

interface DialogProps {
  title: string;
  children: ReactNode;
  /** 底部按钮区 */
  actions: ReactNode;
  /** 危险操作的对话框（红色边框，读屏按警示框处理） */
  danger?: boolean;
  /** 按 Esc 或点遮罩时调用；不传则不能这样关闭（用于必须明确选择的情况） */
  onDismiss?: () => void;
  /** 宽度：默认 480px；lg 为 720px（步骤列表等）；xl 为 960px（多步骤向导、预览） */
  size?: "md" | "lg" | "xl";
}

/**
 * 模态对话框：打开时把焦点放进框内，Tab 不会跑出去。
 * 只在打开时聚焦一次：关闭回调经 ref 读取，父组件重渲染（每次按键、定时刷新）不会把焦点抢走。
 */
export function Dialog({ title, children, actions, danger, onDismiss, size = "md" }: DialogProps) {
  const titleId = useId();
  const ref = useRef<HTMLDivElement>(null);
  const dismissRef = useRef(onDismiss);
  dismissRef.current = onDismiss;

  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    // 内容里已有 autoFocus 的控件就不覆盖它
    if (!ref.current?.contains(document.activeElement)) {
      ref.current?.querySelector<HTMLElement>("button, input, select, textarea, [tabindex]")?.focus();
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && dismissRef.current) dismissRef.current();
      if (e.key === "Tab" && ref.current) {
        const f = Array.from(ref.current.querySelectorAll<HTMLElement>("button:not(:disabled), input, select, textarea, [tabindex]"));
        if (!f.length) return;
        const first = f[0];
        const last = f[f.length - 1];
        if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
        else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("keydown", onKey);
      prev?.focus();
    };
  }, []);

  return createPortal(
    <div className="pl-overlay" onMouseDown={(e) => e.target === e.currentTarget && dismissRef.current?.()}>
      <div
        ref={ref}
        role={danger ? "alertdialog" : "dialog"}
        aria-modal="true"
        aria-labelledby={titleId}
        className={`pl-dialog${danger ? " pl-dialog--danger" : ""}${size !== "md" ? ` pl-dialog--${size}` : ""}`}
      >
        <h2 id={titleId} className="pl-dialog__title">{title}</h2>
        <div className="pl-dialog__body">{children}</div>
        <div className="pl-dialog__actions">{actions}</div>
      </div>
    </div>,
    document.body,
  );
}
