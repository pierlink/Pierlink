import { createPortal } from "react-dom";
import type { ReactNode } from "react";

let root: HTMLElement | null = null;

/** 全局提示区：窗口底部中间，各条提示从下往上排，不互相叠；区域本身不挡点击 */
function noticeRoot(): HTMLElement {
  if (!root) {
    root = document.createElement("div");
    root.className = "notice-stack";
    document.body.appendChild(root);
  }
  return root;
}

/**
 * 把一条浮动提示放进全局提示区（断网、有新版本、加载失败等），不占页面布局、不压住标签栏。
 * @param children 提示内容（自带背景和边框）
 * @param order 排序：数字小的离底部近
 */
export function Notice({ children, order = 0 }: { children: ReactNode; order?: number }) {
  return createPortal(<div className="notice-stack__item" style={{ order }}>{children}</div>, noticeRoot());
}
