import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Check, ChevronDown } from "lucide-react";

export interface SelectOption { value: string; label: string }

interface SelectProps {
  value: string;
  onChange: (value: string) => void;
  options: SelectOption[];
  /** 无障碍名称（没有 labelledBy 时用） */
  label?: string;
  /** 由外面的标签元素提供名称时传它的 id */
  labelledBy?: string;
  disabled?: boolean;
  className?: string;
  /** 每次打开列表时调用（选项可能变了，比如重新读取本机地址） */
  onOpen?: () => void;
}

/**
 * 下拉选择（自绘）：外观和弹出列表都与应用的菜单一致（圆角、阴影、当前项打勾）。
 * 代替原生 select：原生的弹出列表在 Windows 上是系统样式的直角框，与主题不一致。
 * 键盘：Enter / 空格 / 上下方向键打开；打开后上下方向键移动，Enter 选择，Esc 关闭。
 */
export function Select({ value, onChange, options, label, labelledBy, disabled, className = "", onOpen }: SelectProps) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const [pos, setPos] = useState<{ left: number; top: number; width: number; up: boolean } | null>(null);
  const btn = useRef<HTMLButtonElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const listId = useId();
  const current = options.find((o) => o.value === value);

  const show = () => {
    if (disabled) return;
    setActive(Math.max(0, options.findIndex((o) => o.value === value)));
    setOpen(true);
    onOpen?.();
  };
  const close = (focus = true) => {
    setOpen(false);
    if (focus) btn.current?.focus();
  };
  const pick = (i: number) => {
    const o = options[i];
    if (o && o.value !== value) onChange(o.value);
    close();
  };

  // 放在按钮下方；下面放不下时放上面
  useLayoutEffect(() => {
    if (!open || !btn.current) return;
    const r = btn.current.getBoundingClientRect();
    const h = Math.min(320, options.length * 32 + 12);
    const up = r.bottom + h + 8 > window.innerHeight && r.top - h - 8 > 0;
    setPos({ left: r.left, top: up ? r.top - 4 : r.bottom + 4, width: r.width, up });
  }, [open, options.length]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as Node;
      if (!list.current?.contains(t) && !btn.current?.contains(t)) close(false);
    };
    const onScroll = (e: Event) => { if (!list.current?.contains(e.target as Node)) close(false); };
    const onAway = () => close(false);
    window.addEventListener("mousedown", onDown, true);
    window.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onAway);
    window.addEventListener("blur", onAway);
    return () => {
      window.removeEventListener("mousedown", onDown, true);
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onAway);
      window.removeEventListener("blur", onAway);
    };
  }, [open]);

  // 当前项滚动到可见
  useEffect(() => {
    if (open) list.current?.querySelector<HTMLElement>(`[data-i="${active}"]`)?.scrollIntoView({ block: "nearest" });
  }, [open, active]);

  const onKey = (e: React.KeyboardEvent) => {
    if (!open) {
      if (["Enter", " ", "ArrowDown", "ArrowUp"].includes(e.key)) { e.preventDefault(); show(); }
      return;
    }
    if (e.key === "ArrowDown") { e.preventDefault(); setActive((i) => Math.min(options.length - 1, i + 1)); }
    else if (e.key === "ArrowUp") { e.preventDefault(); setActive((i) => Math.max(0, i - 1)); }
    else if (e.key === "Home") { e.preventDefault(); setActive(0); }
    else if (e.key === "End") { e.preventDefault(); setActive(options.length - 1); }
    else if (e.key === "Enter" || e.key === " ") { e.preventDefault(); pick(active); }
    else if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); close(); }
    else if (e.key === "Tab") close(false);
  };

  return (
    <>
      <button
        ref={btn}
        type="button"
        role="combobox"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={open ? listId : undefined}
        aria-label={labelledBy ? undefined : label}
        aria-labelledby={labelledBy}
        aria-activedescendant={open ? `${listId}-${active}` : undefined}
        disabled={disabled}
        className={`pl-select pl-select--custom ${className}`.trim()}
        onClick={() => (open ? close() : show())}
        onKeyDown={onKey}
      >
        <span className="pl-select__value">{current?.label ?? ""}</span>
        <ChevronDown size={14} className="pl-select__chev" aria-hidden="true" />
      </button>
      {open && pos && createPortal(
        <div
          ref={list}
          id={listId}
          role="listbox"
          aria-label={label}
          className="pl-menu pl-select__list"
          style={{ left: pos.left, top: pos.top, minWidth: pos.width, transform: pos.up ? "translateY(-100%)" : undefined }}
          onMouseDown={(e) => e.preventDefault()}
        >
          {options.map((o, i) => (
            <div
              key={o.value}
              id={`${listId}-${i}`}
              data-i={i}
              role="option"
              aria-selected={o.value === value}
              className={`pl-menu__item${i === active ? " pl-select__opt--active" : ""}`}
              onMouseEnter={() => setActive(i)}
              onClick={() => pick(i)}
            >
              <span className="pl-menu__check" aria-hidden="true">{o.value === value ? <Check size={13} /> : null}</span>
              <span className="pl-menu__label">{o.label}</span>
            </div>
          ))}
        </div>,
        document.body,
      )}
    </>
  );
}
