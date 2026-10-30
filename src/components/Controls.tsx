import { useId, useRef, useState, type ComponentProps, type ReactNode, type InputHTMLAttributes } from "react";
import { ChevronDown } from "lucide-react";
import { Menu, type MenuEntry } from "./Menu";
import { Select } from "./Select";

type SelectProps = ComponentProps<typeof Select>;

/** 开关。label 作为无障碍名称。 */
export function Switch({ checked, onChange, label, disabled }: { checked: boolean; onChange: (v: boolean) => void; label: string; disabled?: boolean }) {
  return (
    <button type="button" role="switch" aria-checked={checked} aria-label={label} disabled={disabled} className="pl-switch" onClick={() => onChange(!checked)}>
      <span className="pl-switch__knob" />
    </button>
  );
}

export type Tone = "neutral" | "accent" | "ok" | "warn" | "danger";

/** 小徽标，用于状态和数量。 */
export function Badge({ tone = "neutral", children }: { tone?: Tone; children: ReactNode }) {
  return <span className={`pl-badge${tone !== "neutral" ? ` pl-badge--${tone}` : ""}`}>{children}</span>;
}

/** 状态点。label 必填，供读屏和悬停提示使用（颜色不能是唯一信息）。 */
export function StatusDot({ tone = "neutral", label }: { tone?: Exclude<Tone, "accent">; label: string }) {
  return <span role="img" aria-label={label} title={label} className={`pl-dot${tone !== "neutral" ? ` pl-dot--${tone}` : ""}`} />;
}

/** 分段选择，用于主导航和面板标签。 */
export function Segmented<T extends string>({ options, value, onChange, label }: { options: { value: T; label: string }[]; value: T; onChange: (v: T) => void; label: string }) {
  return (
    <div role="tablist" aria-label={label} className="pl-seg">
      {options.map((o) => (
        <button key={o.value} type="button" role="tab" aria-selected={o.value === value} className="pl-seg__item" onClick={() => onChange(o.value)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

/** 拆分按钮：主区域执行默认动作，右侧小箭头弹出其他选项（如“用其他编辑器打开”）。 */
export function SplitButton({ label, icon, onClick, menuLabel, entries, moreLabel, disabled, title }: { label: string; icon?: ReactNode; onClick: () => void; menuLabel: string; entries: MenuEntry[]; moreLabel: string; disabled?: boolean; title?: string }) {
  const ref = useRef<HTMLButtonElement>(null);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  return (
    <span className="pl-split">
      <button type="button" className="pl-split__main" onClick={onClick} disabled={disabled} title={title}>{icon}{label}</button>
      <button
        ref={ref}
        type="button"
        className="pl-split__more"
        aria-label={moreLabel}
        disabled={disabled}
        aria-haspopup="menu"
        aria-expanded={!!menu}
        onClick={() => {
          const r = ref.current!.getBoundingClientRect();
          setMenu({ x: r.left, y: r.bottom + 4 });
        }}
      >
        <ChevronDown size={12} aria-hidden="true" />
      </button>
      {menu && <Menu x={menu.x} y={menu.y} entries={entries} label={menuLabel} onClose={() => setMenu(null)} />}
    </span>
  );
}

/** 带标签的输入框 */
export function TextField({ label, ...rest }: { label: string } & InputHTMLAttributes<HTMLInputElement>) {
  return (
    <label className="pl-field">
      {label}
      <input className="pl-input" {...rest} />
    </label>
  );
}

/** 带标签的下拉选择 */
export function SelectField({ label, ...rest }: { label: string } & Omit<SelectProps, "label" | "labelledBy">) {
  const id = useId();
  return (
    <div className="pl-field">
      <span id={id}>{label}</span>
      <Select labelledBy={id} {...rest} />
    </div>
  );
}

/** 复选框 */
export function Checkbox({ label, ...rest }: { label: string } & InputHTMLAttributes<HTMLInputElement>) {
  return (
    <label className="pl-check">
      <input type="checkbox" {...rest} />
      {label}
    </label>
  );
}

/** 提示条：信息、成功、警告、危险 */
export function Banner({ tone = "info", title, children, actions }: { tone?: "info" | "ok" | "warn" | "danger"; title: string; children?: ReactNode; actions?: ReactNode }) {
  return (
    <div role={tone === "danger" || tone === "warn" ? "alert" : "status"} className={`pl-banner pl-banner--${tone}`}>
      <div style={{ flex: 1, minWidth: 0, display: "flex", flexDirection: "column", gap: 4 }}>
        <span className="pl-banner__title">{title}</span>
        {children && <span className="pl-banner__body">{children}</span>}
      </div>
      {actions && <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>{actions}</div>}
    </div>
  );
}
