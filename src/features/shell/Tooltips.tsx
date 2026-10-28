import { useEffect, useRef, useState } from "react";
import "./tooltips.css";

/** 提示文字存放的属性：元素原来的 title 移到这里，避免浏览器再显示自带的提示 */
export const TIP_ATTR = "data-pl-tip";
const SHOW_DELAY = 500;
/** 刚显示过提示时，移到下一个按钮上立即显示（不再等） */
const WARM_MS = 800;
const MARGIN = 8;

/** 从 target 往上找带提示的元素；遇到 title 就搬到 data-pl-tip（终端内容区里的不管） */
function tipTarget(target: EventTarget | null): HTMLElement | null {
  if (!(target instanceof Element)) return null;
  const el = target.closest<HTMLElement>(`[title], [${TIP_ATTR}]`);
  if (!el || el.closest(".xterm-screen")) return null;
  const title = el.getAttribute("title");
  if (title !== null) {
    el.removeAttribute("title");
    if (title) el.setAttribute(TIP_ATTR, title);
    else el.removeAttribute(TIP_ATTR);
  }
  return el.getAttribute(TIP_ATTR) ? el : null;
}

/** 比这宽的元素，提示放在鼠标下面而不是元素中间 */
const WIDE = 240;

interface Tip { text: string; x: number; y: number; below: boolean }

/**
 * 全局鼠标提示，代替浏览器自带的 title 提示。
 * 自带提示的问题：点过的按钮要移出再移入才会再显示；样式也和主题不一致。
 * 这里统一接管所有带 title 的元素：悬停 0.5 秒显示；点击时先隐藏，鼠标再动一下又会显示。
 */
export function Tooltips() {
  const [tip, setTip] = useState<Tip | null>(null);
  const bubble = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let current: HTMLElement | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let lastShown = 0;
    let shown = false;
    // 鼠标最后的横坐标：很宽的元素（整行、表格行）提示跟着鼠标，不跑到元素正中间
    let pointerX = 0;
    const clear = () => { if (timer) { clearTimeout(timer); timer = null; } };
    const hide = () => {
      clear();
      if (shown) lastShown = Date.now();
      shown = false;
      setTip(null);
    };
    const show = () => {
      timer = null;
      const el = current;
      const text = el?.getAttribute(TIP_ATTR);
      if (!el || !text || !el.isConnected) return;
      const r = el.getBoundingClientRect();
      const below = r.bottom + 40 < window.innerHeight;
      shown = true;
      const x = r.width > WIDE ? Math.min(Math.max(pointerX, r.left), r.right) : r.left + r.width / 2;
      setTip({ text, x, y: below ? r.bottom + 6 : r.top - 6, below });
    };
    const arm = () => {
      clear();
      timer = setTimeout(show, Date.now() - lastShown < WARM_MS ? 0 : SHOW_DELAY);
    };
    const onOver = (e: PointerEvent) => {
      pointerX = e.clientX;
      const el = tipTarget(e.target);
      if (el === current) return;
      hide();
      current = el;
      if (el) arm();
    };
    const onMove = (e: PointerEvent) => {
      pointerX = e.clientX;
      // 文字变了（React 重新写回了 title）时再搬一次；点击后隐藏的提示，鼠标再动就重新计时
      const el = tipTarget(e.target);
      if (el !== current) { onOver(e); return; }
      if (el && !shown && !timer) arm();
    };
    const onOut = (e: PointerEvent) => {
      if (!current) return;
      const to = e.relatedTarget;
      if (to instanceof Node && current.contains(to)) return;
      hide();
      current = null;
    };
    const onDown = () => hide();
    const onKey = () => hide();
    const onScroll = () => hide();
    document.addEventListener("pointerover", onOver, true);
    document.addEventListener("pointermove", onMove, true);
    document.addEventListener("pointerout", onOut, true);
    document.addEventListener("pointerdown", onDown, true);
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("wheel", onScroll, { capture: true, passive: true });
    window.addEventListener("blur", hide);
    return () => {
      clear();
      document.removeEventListener("pointerover", onOver, true);
      document.removeEventListener("pointermove", onMove, true);
      document.removeEventListener("pointerout", onOut, true);
      document.removeEventListener("pointerdown", onDown, true);
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("wheel", onScroll, true);
      window.removeEventListener("blur", hide);
    };
  }, []);

  // 先按中心位置渲染，量出宽度后再夹在窗口内
  useEffect(() => {
    const el = bubble.current;
    if (!tip || !el) return;
    const w = el.offsetWidth;
    const left = Math.min(Math.max(tip.x - w / 2, MARGIN), window.innerWidth - w - MARGIN);
    el.style.left = `${left}px`;
    el.style.visibility = "visible";
  }, [tip]);

  if (!tip) return null;
  return (
    <div
      ref={bubble}
      role="tooltip"
      className={`pl-tip${tip.below ? "" : " pl-tip--above"}`}
      style={{ top: tip.y, left: tip.x, visibility: "hidden" }}
    >
      {tip.text}
    </div>
  );
}
