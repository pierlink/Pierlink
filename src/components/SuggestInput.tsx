import { useEffect, useId, useLayoutEffect, useRef, useState, type InputHTMLAttributes } from "react";
import { createPortal } from "react-dom";

type Props = Omit<InputHTMLAttributes<HTMLInputElement>, "value" | "onChange"> & {
  value: string;
  onChange: (value: string) => void;
  /** 候选项（如已有的分组名） */
  suggestions: string[];
};

/**
 * 可以自由输入、也可以从候选里选的文本框（代替原生 datalist，弹出列表与应用菜单同一风格）。
 * 获得焦点或输入时显示匹配的候选；上下方向键移动，Enter 选择，Esc 收起。
 */
export function SuggestInput({ value, onChange, suggestions, className = "pl-input", ...rest }: Props) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const [pos, setPos] = useState<{ left: number; top: number; width: number } | null>(null);
  const input = useRef<HTMLInputElement>(null);
  const listId = useId();
  const q = value.trim().toLowerCase();
  const items = suggestions.filter((s) => s !== value && (!q || s.toLowerCase().includes(q))).slice(0, 50);
  const visible = open && items.length > 0;

  useLayoutEffect(() => {
    if (!visible || !input.current) return;
    const r = input.current.getBoundingClientRect();
    setPos({ left: r.left, top: r.bottom + 4, width: r.width });
  }, [visible, value]);
  useEffect(() => { setActive(-1); }, [value]);

  const pick = (s: string) => { onChange(s); setOpen(false); };

  return (
    <>
      <input
        ref={input}
        {...rest}
        className={className}
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={visible}
        aria-controls={visible ? listId : undefined}
        aria-activedescendant={visible && active >= 0 ? `${listId}-${active}` : undefined}
        value={value}
        onChange={(e) => { onChange(e.target.value); setOpen(true); }}
        onFocus={(e) => { setOpen(true); rest.onFocus?.(e); }}
        onBlur={(e) => { setOpen(false); rest.onBlur?.(e); }}
        onKeyDown={(e) => {
          if (visible && e.key === "ArrowDown") { e.preventDefault(); setActive((i) => Math.min(items.length - 1, i + 1)); return; }
          if (visible && e.key === "ArrowUp") { e.preventDefault(); setActive((i) => Math.max(0, i - 1)); return; }
          if (visible && e.key === "Enter" && active >= 0) { e.preventDefault(); pick(items[active]); return; }
          if (visible && e.key === "Escape") { e.preventDefault(); e.stopPropagation(); setOpen(false); return; }
          if (!visible && e.key === "ArrowDown") { setOpen(true); return; }
          rest.onKeyDown?.(e);
        }}
      />
      {visible && pos && createPortal(
        <div id={listId} role="listbox" className="pl-menu pl-select__list" style={{ left: pos.left, top: pos.top, minWidth: pos.width }} onMouseDown={(e) => e.preventDefault()}>
          {items.map((s, i) => (
            <div key={s} id={`${listId}-${i}`} role="option" aria-selected={i === active} className={`pl-menu__item${i === active ? " pl-select__opt--active" : ""}`} onMouseEnter={() => setActive(i)} onClick={() => pick(s)}>
              <span className="pl-menu__label">{s}</span>
            </div>
          ))}
        </div>,
        document.body,
      )}
    </>
  );
}
