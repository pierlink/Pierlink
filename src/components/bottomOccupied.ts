import { useEffect, useSyncExternalStore } from "react";

/**
 * 占着窗口底部的浮层个数：底部提示条、文件面板的删除浮窗、盖满窗口的文本编辑器。
 * 右下角的更新卡片在有它们时先不显示，免得盖住“停止删除”这类按钮，或者和提示叠在一起看不清。
 */
let count = 0;
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((l) => l());
const subscribe = (l: () => void) => {
  listeners.add(l);
  return () => { listeners.delete(l); };
};
const occupied = () => count > 0;

/**
 * 组件挂载期间占着窗口底部。
 * @param active 为 false 时不算（比如删除浮窗里没有任务）
 */
export function useOccupyBottom(active = true): void {
  useEffect(() => {
    if (!active) return;
    count += 1;
    emit();
    return () => {
      count -= 1;
      emit();
    };
  }, [active]);
}

/** 有没有浮层占着窗口底部 */
export function useBottomOccupied(): boolean {
  return useSyncExternalStore(subscribe, occupied);
}
