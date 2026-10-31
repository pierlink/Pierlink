import { useEffect } from "react";

/**
 * 软键盘弹出时可见区域变小：iOS 不缩小布局视口，底部的输入框会被键盘挡住。
 * 把可见区域的高度和顶部偏移写成 CSS 变量（--m-vh、--m-vtop），全屏的终端页按它排版。
 */
export function useVisualViewport(): void {
  useEffect(() => {
    const vv = window.visualViewport;
    const root = document.documentElement;
    if (!vv) return;
    let frame = 0;
    const update = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        root.style.setProperty("--m-vh", `${Math.round(vv.height)}px`);
        root.style.setProperty("--m-vtop", `${Math.round(vv.offsetTop)}px`);
      });
    };
    update();
    vv.addEventListener("resize", update);
    vv.addEventListener("scroll", update);
    return () => {
      cancelAnimationFrame(frame);
      vv.removeEventListener("resize", update);
      vv.removeEventListener("scroll", update);
      root.style.removeProperty("--m-vh");
      root.style.removeProperty("--m-vtop");
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
