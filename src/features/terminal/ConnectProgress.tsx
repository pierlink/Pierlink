import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, X } from "lucide-react";
import type { ConnectStage } from "./types";

/** 界面上的四步；ready 不单独显示，收到就收起进度 */
export const STEPS = ["resolve", "connect", "auth", "session"] as const;
export type Step = (typeof STEPS)[number];

/** 连得快时不显示进度，免得一闪而过（毫秒） */
const SHOW_AFTER = 250;

/**
 * 阶段的先后，用来忽略倒退的阶段（后端保证顺序，这里再防一层）。
 * @param stage 阶段
 * @returns 序号；ready 排在最后
 */
export function stageIndex(stage: ConnectStage): number {
  return stage === "ready" ? STEPS.length : STEPS.indexOf(stage);
}

/**
 * 四个节点的进度：走过的打勾，当前的高亮，停住的标红。
 * @param stage 当前一步（或停在的那一步）
 * @param failed 停在这一步没走下去
 */
export function ConnectSteps({ stage, failed = false }: { stage: Step; failed?: boolean }) {
  const { t } = useTranslation();
  const at = STEPS.indexOf(stage);
  return (
    <ol className="conn-steps" aria-label={t("terminal.stages.label")}>
      {STEPS.map((s, i) => {
        const state = i < at ? "done" : i > at ? "todo" : failed ? "failed" : "now";
        return (
          <li key={s} className={`conn-step conn-step--${state}`} aria-current={state === "now" ? "step" : undefined}>
            <span className="conn-step__dot" aria-hidden="true">
              {state === "done" ? <Check size={11} strokeWidth={3} /> : state === "failed" ? <X size={11} strokeWidth={3} /> : null}
            </span>
            <span className="conn-step__label">{t(`terminal.stages.${s}`)}</span>
          </li>
        );
      })}
    </ol>
  );
}

/**
 * 连接中：盖在空终端中间的四步进度和已用秒数（小数点后一位）。
 * @param stage 当前一步
 * @param start 开始连接的时间（performance.now()）
 */
export function ConnectProgress({ stage, start }: { stage: Step; start: number }) {
  const { t } = useTranslation();
  const [now, setNow] = useState(() => performance.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(performance.now()), 100);
    return () => clearInterval(timer);
  }, []);
  const elapsed = Math.max(0, now - start);
  if (elapsed < SHOW_AFTER) return null;
  return (
    <div className="conn-progress" role="status">
      <ConnectSteps stage={stage} />
      <div className="conn-progress__time">
        {t("terminal.stages.connecting")}
        <span className="conn-progress__secs mono" aria-hidden="true">{t("terminal.stages.seconds", { s: (elapsed / 1000).toFixed(1) })}</span>
      </div>
    </div>
  );
}
