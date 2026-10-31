import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Button } from "../../components";
import type { ConnectStage } from "./types";

/**
 * 界面上的四步：连接（解析地址、建立 TCP）、验证主机（核对指纹）、登录、启动终端。
 * 后端报的阶段按 stepOf 换算；ready 不单独显示，收到就收起。
 */
export const STEPS = ["connect", "verify", "auth", "session"] as const;
export type Step = (typeof STEPS)[number];

/** 连得很快时不显示，免得一闪而过（毫秒） */
const SHOW_AFTER = 120;
/** 当前这一步的进度条按时间渐近填充：时间常数（毫秒）和最多填到多少 */
const FILL_TAU = 1400;
const FILL_MAX = 0.92;

/**
 * 后端的连接阶段换算成界面上的一步。
 * @param stage 后端报的阶段
 * @returns 界面上的一步；ready 返回 null（连接完成）
 */
export function stepOf(stage: ConnectStage): Step | null {
  switch (stage) {
    case "resolve":
    case "connect":
      return "connect";
    case "auth":
      return "auth";
    case "session":
      return "session";
    default:
      return null;
  }
}

/** 一步在 STEPS 里的位置 */
export function stepIndex(step: Step): number {
  return STEPS.indexOf(step);
}

/**
 * 当前这一步填了多少（0–FILL_MAX）：开头快、越往后越慢，一直在动但不会填满，走到下一步时才补满。
 * @param ms 这一步已经走了多少毫秒
 */
export function stepFill(ms: number): number {
  return FILL_MAX * (1 - Math.exp(-Math.max(0, ms) / FILL_TAU));
}

/** 每帧刷新一次的当前时间（performance.now()）；不在页面上显示时浏览器会自动停下 */
function useFrameClock(): number {
  const [now, setNow] = useState(() => performance.now());
  useEffect(() => {
    let frame = requestAnimationFrame(function tick(t) {
      setNow(t);
      frame = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(frame);
  }, []);
  return now;
}

/**
 * 四段进度条和每段的名字：走过的填满，当前的随时间渐近填充，停住的标红。
 * @param at 当前一步（或停在的那一步）
 * @param failed 停在这一步没走下去（静态显示，不动画）
 * @param fill 当前一步的填充比例（0–1）
 */
export function ConnectSteps({ at, failed = false, fill = 0, done = false }: { at: Step; failed?: boolean; fill?: number; done?: boolean }) {
  const { t } = useTranslation();
  // 连好了：所有段都算走过
  const current = done ? STEPS.length : stepIndex(at);
  return (
    <ol className="conn-track" aria-label={t("terminal.stages.label")}>
      {STEPS.map((s, i) => {
        const state = i < current ? "done" : i > current ? "todo" : failed ? "failed" : "now";
        const width = state === "done" || state === "failed" ? 1 : state === "now" ? fill : 0;
        return (
          <li key={s} className={`conn-seg conn-seg--${state}`} aria-current={state === "now" ? "step" : undefined}>
            <span className="conn-seg__bar" aria-hidden="true">
              <span className="conn-seg__fill" style={{ transform: `scaleX(${width})` }} />
            </span>
            <span className="conn-seg__label">{t(`terminal.stages.${s}`)}</span>
          </li>
        );
      })}
    </ol>
  );
}

/**
 * 正在连接的卡片：服务器名、已用秒数（小数点后一位）、四段进度条和“取消”。
 * 从点连接开始一直是这一张：先核对指纹，再登录、启动终端，到出现提示符或终端有了输出才收起。
 * @param title 服务器名
 * @param at 当前一步
 * @param stepStart 当前一步开始的时间（performance.now()）
 * @param start 开始连接的时间（performance.now()）
 * @param done 已经连好：四段填满并淡出
 * @param onCancel 点“取消”（不给就不显示按钮）
 */
export function ConnectCard({ title, at, stepStart, start, done = false, onCancel }: { title: string; at: Step; stepStart: number; start: number; done?: boolean; onCancel?: () => void }) {
  const { t } = useTranslation();
  const now = useFrameClock();
  const elapsed = Math.max(0, now - start);
  // 播报区域一开始就在，内容晚一点出来：读屏软件才会念（动态插入的播报区域很多读屏不念）
  return (
    <div className="conn-card-wrap" role="status">
      {elapsed >= SHOW_AFTER && (
        <div className={`conn-card${done ? " conn-card--done" : ""}`}>
          <div className="conn-card__head">
            <span className="conn-card__title pl-ellipsis">{t("terminal.connectingTo", { name: title })}</span>
            <span className="conn-card__secs mono" aria-hidden="true">{t("terminal.stages.seconds", { s: (elapsed / 1000).toFixed(1) })}</span>
          </div>
          <ConnectSteps at={at} fill={stepFill(now - stepStart)} done={done} />
          {onCancel && (
            <div className="conn-card__actions">
              <Button size="sm" variant="ghost" onClick={onCancel}>{t("common.cancel")}</Button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
