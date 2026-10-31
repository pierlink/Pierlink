import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";

/** 菜单项：普通项、分隔线、标题、说明文字 */
export type MenuEntry =
  | {
      kind?: "item";
      id: string;
      label: string;
      /** 快捷键提示，如 "Ctrl Shift C" */
      shortcut?: string;
      icon?: ReactNode;
      checked?: boolean;
      danger?: boolean;
      disabled?: boolean;
      onSelect?: () => void;
    }
  | { kind: "separator"; id: string }
  | { kind: "header"; id: string; label: string }
  | { kind: "hint"; id: string; label: string };

interface MenuProps {
  /** 菜单左上角的屏幕坐标 */
  x: number;
  y: number;
  entries: MenuEntry[];
  /** 菜单的无障碍名称 */
  label: string;
  onClose: () => void;
}

/**
 * 弹出菜单（下拉和右键共用）。支持方向键、Enter、Esc，点击外部关闭，自动避开屏幕边缘。
 */
export function Menu({ x, y, entries, label, onClose }: MenuProps) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ left: x, top: y });

  // 超出窗口时向内收回
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const left = Math.max(8, Math.min(x, window.innerWidth - r.width - 8));
    const top = Math.max(8, Math.min(y, window.innerHeight - r.height - 8));
    setPos({ left, top });
  }, [x, y]);

  useEffect(() => {
    // 焦点先放在菜单本身：用鼠标打开时不高亮任何一项；按方向键再进入第一项/最后一项
    ref.current?.focus();
    const onDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("mousedown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    window.addEventListener("blur", onClose);
    return () => {
      window.removeEventListener("mousedown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
      window.removeEventListener("blur", onClose);
    };
  }, [onClose]);

  const moveFocus = (dir: 1 | -1) => {
    const items = Array.from(ref.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not([aria-disabled="true"])') ?? []);
    if (!items.length) return;
    const i = items.indexOf(document.activeElement as HTMLButtonElement);
    const next = i < 0 ? (dir > 0 ? 0 : items.length - 1) : (i + dir + items.length) % items.length;
    items[next].focus();
  };

  return createPortal(
    <div
      ref={ref}
      role="menu"
      tabIndex={-1}
      aria-label={label}
      className="pl-menu"
      style={{ left: pos.left, top: pos.top }}
      onKeyDown={(e) => {
        if (e.key === "ArrowDown") { e.preventDefault(); moveFocus(1); }
        if (e.key === "ArrowUp") { e.preventDefault(); moveFocus(-1); }
      }}
      onContextMenu={(e) => e.preventDefault()}
    >
      {entries.map((m) => {
        if (m.kind === "separator") return <div key={m.id} role="separator" className="pl-menu__sep" />;
        if (m.kind === "header") return <div key={m.id} className="pl-menu__header">{m.label}</div>;
        if (m.kind === "hint") return <div key={m.id} className="pl-menu__hint">{m.label}</div>;
        return (
          <button
            key={m.id}
            type="button"
            role="menuitem"
            aria-disabled={m.disabled || undefined}
            className={`pl-menu__item${m.danger ? " pl-menu__item--danger" : ""}`}
            onClick={() => {
              if (m.disabled) return;
              onClose();
              m.onSelect?.();
            }}
          >
            <span className="pl-menu__check" aria-hidden="true">{m.checked ? "✓" : m.icon}</span>
            <span className="pl-menu__label">{m.label}</span>
            {m.shortcut && <span className="pl-menu__key">{m.shortcut}</span>}
          </button>
        );
      })}
    </div>,
    document.body,
  );
}

/**
 * 右键菜单的状态管理：返回打开函数和要渲染的菜单节点。
 * @param label 菜单的无障碍名称
 */
export function useContextMenu(label: string) {
  const [state, setState] = useState<{ x: number; y: number; entries: MenuEntry[] } | null>(null);
  const open = (e: { clientX: number; clientY: number; preventDefault: () => void }, entries: MenuEntry[]) => {
    e.preventDefault();
    setState({ x: e.clientX, y: e.clientY, entries });
  };
  const node = state ? <Menu x={state.x} y={state.y} entries={state.entries} label={label} onClose={() => setState(null)} /> : null;
  return { open, node };
}
