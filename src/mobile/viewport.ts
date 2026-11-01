import { useEffect } from "react";

/** 焦点在能打字的地方（输入框、文本框）：这时变矮才是键盘 */
function typing(): boolean {
  const a = document.activeElement;
  if (a instanceof HTMLTextAreaElement) return true;
  return a instanceof HTMLInputElement && !["button", "checkbox", "radio", "range", "submit", "reset", "file", "color"].includes(a.type);
}

/**
 * 软键盘弹出时可见区域变小：iOS 不缩小布局视口，底部的输入框会被键盘挡住。
 * 把可见区域的高度和顶部偏移写成 CSS 变量（--m-vh、--m-vtop），整页按它排版；
 * 键盘开着时在根元素上标 data-kb：看终端时底部标签让位，键盘收起后标签回来。
 * 只有输入框有焦点时才算键盘开着（工具栏伸缩、iOS 的高度报错不会误判）；收起时把 iOS 留下的页面滚动清掉。
 */
export function useVisualViewport(): void {
  useEffect(() => {
    const vv = window.visualViewport;
    const root = document.documentElement;
    if (!vv) return;
    let frame = 0;
    // 这个宽度下见过的最大高度：比它矮一截（键盘的高度）就是键盘开着；转屏后重新算
    let tallest = 0;
    let width = 0;
    const update = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        if (Math.round(vv.width) !== width) { width = Math.round(vv.width); tallest = 0; }
        tallest = Math.max(tallest, vv.height);
        root.style.setProperty("--m-vh", `${Math.round(vv.height)}px`);
        root.style.setProperty("--m-vtop", `${Math.round(vv.offsetTop)}px`);
        if (typing() && tallest - vv.height > 120) root.dataset.kb = "1";
        else if (root.dataset.kb) {
          delete root.dataset.kb;
          window.scrollTo(0, 0);
        }
      });
    };
    update();
    vv.addEventListener("resize", update);
    vv.addEventListener("scroll", update);
    document.addEventListener("focusin", update);
    document.addEventListener("focusout", update);
    return () => {
      cancelAnimationFrame(frame);
      vv.removeEventListener("resize", update);
      vv.removeEventListener("scroll", update);
      document.removeEventListener("focusin", update);
      document.removeEventListener("focusout", update);
      root.style.removeProperty("--m-vh");
      root.style.removeProperty("--m-vtop");
      delete root.dataset.kb;
    };
  }, []);
}

/**
 * 看终端时尽量不让屏幕熄灭（只有 HTTPS 或 localhost 才有 Wake Lock；没有就算了）。
 * @param active 是否在看终端
 */
export function useWakeLock(active: boolean): void {
  useEffect(() => {
    if (!active || !("wakeLock" in navigator)) return;
    let lock: WakeLockSentinel | null = null;
    let alive = true;
    const request = () => {
      if (document.visibilityState !== "visible") return;
      navigator.wakeLock.request("screen").then((l) => { if (alive) lock = l; else void l.release(); }, () => {});
    };
    request();
    // 切到后台会自动释放：回来时再要一次
    document.addEventListener("visibilitychange", request);
    return () => {
      alive = false;
      document.removeEventListener("visibilitychange", request);
      void lock?.release().catch(() => {});
    };
  }, [active]);
}
