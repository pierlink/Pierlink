import type { WheelEvent } from "react";
import { useTranslation } from "react-i18next";
import { Button } from "../../components";

/**
 * 断开、退出后放在终端正中间的卡片（和连接卡片一样的位置和样式）：说明、“关闭”“重新连接”，按 Enter 也能重新连接。
 * 外层不挡鼠标：卡片以外的终端内容照样能滚动、选中复制。点卡片上按钮以外的地方把焦点给终端，接着按 Enter 照样重新连接。
 * @param title 说明（连接已断开、已断开、进程已退出）
 * @param detail 补充说明（没有就不显示）
 * @param warn 连接意外断开：标题前的圆点和边框用警告色
 * @param closeLabel “关闭”按钮的文字
 * @param onClose 点“关闭”（不给就不显示按钮）
 * @param reopenLabel “重新连接”或“重新打开”
 * @param onReopen 点“重新连接”
 * @param onFocusTerminal 把焦点给终端
 * @param onWheel 鼠标停在卡片上滚动滚轮（转给终端，照样能往上翻）
 */
export function EndCard({ title, detail, warn, closeLabel, onClose, reopenLabel, onReopen, onFocusTerminal, onWheel }: {
  title: string;
  detail?: string | null;
  warn: boolean;
  closeLabel: string;
  onClose?: () => void;
  reopenLabel: string;
  onReopen: () => void;
  onFocusTerminal: () => void;
  onWheel: (e: WheelEvent) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="term-endcard-wrap">
      <div
        className={`term-endcard${warn ? " term-endcard--warn" : ""}`}
        role="alert"
        onMouseDown={(e) => {
          if ((e.target as Element).closest("button")) return;
          // 不让焦点落到卡片上（落上去后按 Enter 没反应）
          e.preventDefault();
          onFocusTerminal();
        }}
        onWheel={onWheel}
      >
        <div className="term-endcard__title">
          <span className="term-endcard__dot" aria-hidden="true" />
          <span className="pl-ellipsis">{title}</span>
        </div>
        {detail && <div className="term-endcard__detail">{detail}</div>}
        <div className="term-endcard__foot">
          <span className="term-endcard__hint">{t("terminal.endedEnterHint")}</span>
          {/* 两个按钮放一组：窗格窄时整组一起换行，不会一上一下 */}
          <span className="term-endcard__btns">
            {onClose && <Button onClick={onClose}>{closeLabel}</Button>}
            <Button variant="primary" onClick={onReopen}>{reopenLabel}</Button>
          </span>
        </div>
      </div>
    </div>
  );
}
