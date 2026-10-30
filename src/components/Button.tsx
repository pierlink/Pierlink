import type { ButtonHTMLAttributes, ReactNode } from "react";

export type ButtonVariant = "primary" | "secondary" | "ghost" | "soft" | "danger" | "danger-ghost";

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  /** 外观：主操作、次要、幽灵、柔和、危险 */
  variant?: ButtonVariant;
  /** 尺寸 */
  size?: "sm" | "md" | "lg";
  /** 文字前的图标 */
  icon?: ReactNode;
}

/** 通用按钮。危险操作用 variant="danger"，并且必须配合二次确认。 */
export function Button({ variant = "secondary", size = "md", icon, className = "", children, type = "button", ...rest }: ButtonProps) {
  const cls = ["pl-btn", `pl-btn--${variant}`, size !== "md" ? `pl-btn--${size}` : "", className].filter(Boolean).join(" ");
  return (
    <button type={type} className={cls} {...rest}>
      {icon}
      {children}
    </button>
  );
}

interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  /** 无障碍名称，必填（图标按钮没有可见文字） */
  label: string;
  size?: "sm" | "md";
  children: ReactNode;
}

/** 只有图标的按钮，label 同时作为悬停提示和读屏名称。 */
export function IconButton({ label, size = "md", className = "", children, type = "button", ...rest }: IconButtonProps) {
  const cls = ["pl-icon-btn", size === "sm" ? "pl-icon-btn--sm" : "", className].filter(Boolean).join(" ");
  return (
    <button type={type} aria-label={label} title={label} className={cls} {...rest}>
      {children}
    </button>
  );
}
