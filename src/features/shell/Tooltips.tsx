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
/** 可停留的提示：鼠标离开元素后留这么久，移进提示框就不关 */
const HOLD_GRACE = 300;
/** 标了这个属性的区域里（如操作记录表格），提示都可以停留、选中复制 */
export const TIP_HOLD_ATTR = "data-pl-tip-hold";

/** 长的、多行的提示，或标了 data-pl-tip-hold 的区域里的提示：鼠标能移进去选中、复制、滚动 */
function holdable(el: HTMLElement, text: string): boolean {
  return !!el.closest(`[${TIP_HOLD_ATTR}]`) || text.length > 80 || text.includes("\n");
}

interface Tip { text: string; x: number; y: number; below: boolean; hold: boolean; /** 元素的上下边（放不下时翻到另一边用） */ top: number; bottom: number; gap: number; flipped?: boolean }

/**
 * 全局鼠标提示，代替浏览器自带的 title 提示。
 * 自带提示的问题：点过的按钮要移出再移入才会再显示；样式也和主题不一致。
 * 这里统一接管所有带 title 的元素：悬停 0.5 秒显示；点击时先隐藏，鼠标再动一下又会显示。
 * 长提示可以停留：鼠标离开元素后 0.3 秒内移进提示框，提示就一直开着，能选中、复制、滚动；
 * 移出提示框（没回到元素上）0.3 秒后关，按 Esc 也关。
 */
export function Tooltips() {
  const [tip, setTip] = useState<Tip | null>(null);
  const bubble = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let current: HTMLElement | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let lastShown = 0;
    let shown = false;
    // 当前显示的提示可以停留；离开后等着关的计时器
    let held = false;
    let leaving: ReturnType<typeof setTimeout> | null = null;
    const inBubble = (node: EventTarget | null) => node instanceof Node && !!bubble.current?.contains(node);
    const stay = () => { if (leaving) { clearTimeout(leaving); leaving = null; } };
    // 鼠标最后的横坐标：很宽的元素（整行、表格行）提示跟着鼠标，不跑到元素正中间
    let pointerX = 0;
    const clear = () => { if (timer) { clearTimeout(timer); timer = null; } };
    const hide = () => {
      clear();
      stay();
      if (shown) lastShown = Date.now();
      shown = false;
      held = false;
      setTip(null);
    };
    /** 可停留的提示过一会再关：期间移进提示框或回到元素上就留着 */
    const leaveSoon = () => {
      stay();
      leaving = setTimeout(() => { leaving = null; hide(); current = null; }, HOLD_GRACE);
    };
    const show = () => {
      timer = null;
      const el = current;
      const text = el?.getAttribute(TIP_ATTR);
      if (!el || !text || !el.isConnected) return;
      const r = el.getBoundingClientRect();
      const below = r.bottom + 40 < window.innerHeight;
      shown = true;
      held = holdable(el, text);
      const x = r.width > WIDE ? Math.min(Math.max(pointerX, r.left), r.right) : r.left + r.width / 2;
      // 可停留的提示贴近元素（只留 2 像素），鼠标移过去的路上不会掉进空隙
      const gap = held ? 2 : 6;
      setTip({ text, x, y: below ? r.bottom + gap : r.top - gap, below, hold: held, top: r.top, bottom: r.bottom, gap });
    };
    const arm = () => {
      clear();
      timer = setTimeout(show, Date.now() - lastShown < WARM_MS ? 0 : SHOW_DELAY);
    };
    const onOver = (e: PointerEvent) => {
      pointerX = e.clientX;
      // 移进可停留的提示框：留着
      if (inBubble(e.target)) { if (held) stay(); return; }
      const el = tipTarget(e.target);
      if (el === current) { stay(); return; }
      // 从可停留的提示的元素移开：等一会，可能正往提示框里移（按着键在选字时不关）
      if (held && shown) { if (!e.buttons) leaveSoon(); return; }
      hide();
      current = el;
      if (el) arm();
    };
    const onMove = (e: PointerEvent) => {
      pointerX = e.clientX;
      if (inBubble(e.target) || leaving) return;
      // 文字变了（React 重新写回了 title）时再搬一次；点击后隐藏的提示，鼠标再动就重新计时
      const el = tipTarget(e.target);
      if (el !== current) { onOver(e); return; }
      if (el && !shown && !timer) arm();
    };
    const onOut = (e: PointerEvent) => {
      const to = e.relatedTarget;
      // 在提示框里面移动、或从提示框回到元素上：不关
      if (inBubble(e.target)) {
        if (inBubble(to) || (to instanceof Node && current?.contains(to)) || e.buttons) return;
        leaveSoon();
        return;
      }
      if (!current) return;
      if (to instanceof Node && current.contains(to)) return;
      if (held && shown) { leaveSoon(); return; }
      hide();
      current = null;
    };
    // React 改了按钮的 title（如“全屏”换成“退出全屏”）会把属性写回去：立刻接管，免得浏览器自带的提示也冒出来
    const mo = new MutationObserver((records) => {
      for (const r of records) {
        if (!(r.target instanceof HTMLElement) || r.target.getAttribute("title") === null || r.target.closest(".xterm-screen")) continue;
        tipTarget(r.target);
        if (r.target === current && shown) show();
      }
    });
    mo.observe(document.body, { subtree: true, attributes: true, attributeFilter: ["title"] });
    // 在可停留的提示框里按下（选文字）、滚动、按 Ctrl+C 都不关；Esc 关
    const onDown = (e: PointerEvent) => { if (!inBubble(e.target)) hide(); };
    const onKey = (e: KeyboardEvent) => {
      if (held && shown && e.key !== "Escape" && (e.ctrlKey || e.metaKey || ["Control", "Meta", "Shift", "Alt"].includes(e.key))) return;
      hide();
    };
    const onScroll = (e: WheelEvent) => { if (!inBubble(e.target)) hide(); };
    document.addEventListener("pointerover", onOver, true);
    document.addEventListener("pointermove", onMove, true);
    document.addEventListener("pointerout", onOut, true);
    document.addEventListener("pointerdown", onDown, true);
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("wheel", onScroll, { capture: true, passive: true });
    window.addEventListener("blur", hide);
    return () => {
      clear();
      mo.disconnect();
      document.removeEventListener("pointerover", onOver, true);
      document.removeEventListener("pointermove", onMove, true);
      document.removeEventListener("pointerout", onOut, true);
      document.removeEventListener("pointerdown", onDown, true);
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("wheel", onScroll, true);
      window.removeEventListener("blur", hide);
    };
  }, []);

  // 先按中心位置渲染，量出宽高后再夹在窗口内；下面放不下（可停留的长提示）时翻到元素上面
  useEffect(() => {
    const el = bubble.current;
    if (!tip || !el) return;
    // 提示已经显示着又换了内容（title 变了）时，量完、摆好之前先藏起来，不会闪一下
    el.style.visibility = "hidden";
    const w = el.offsetWidth;
    const left = Math.min(Math.max(tip.x - w / 2, MARGIN), window.innerWidth - w - MARGIN);
    el.style.left = `${left}px`;
    const h = el.offsetHeight;
    const roomBelow = window.innerHeight - MARGIN - (tip.bottom + tip.gap);
    const roomAbove = tip.top - tip.gap - MARGIN;
    if (tip.below && !tip.flipped && h > roomBelow && roomAbove > roomBelow) {
      // 换成上面再渲染一次（还没显示，不会闪）
      setTip({ ...tip, below: false, y: tip.top - tip.gap, flipped: true });
      return;
    }
    el.style.visibility = "visible";
  }, [tip]);

  if (!tip) return null;
  return (
    <div
      ref={bubble}
      role="tooltip"
      className={`pl-tip${tip.below ? "" : " pl-tip--above"}${tip.hold ? " pl-tip--hold" : ""}`}
      style={{ top: tip.y, left: tip.x, visibility: "hidden" }}
    >
      {tip.text}
    </div>
  );
}
