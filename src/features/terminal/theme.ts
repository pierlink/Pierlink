import type { ITheme } from "@xterm/xterm";

/** 终端配色（与界面深浅主题对应）。背景透明，透出面板颜色。 */
const dark: ITheme = {
  background: "rgba(0,0,0,0)", foreground: "#e7e9f0", cursor: "#929dff", cursorAccent: "#12131a",
  selectionBackground: "rgba(146,157,255,0.35)",
  black: "#1b1e27", red: "#ff8d8d", green: "#7fe0a7", yellow: "#f3c77a", blue: "#86aaff", magenta: "#c4a2ff", cyan: "#7fd8e0", white: "#d5d9e3",
  brightBlack: "#6f7689", brightRed: "#ffb0b0", brightGreen: "#a8f0c4", brightYellow: "#f8daa2", brightBlue: "#aac4ff", brightMagenta: "#d9c2ff", brightCyan: "#a8e8ee", brightWhite: "#ffffff",
};

const light: ITheme = {
  background: "rgba(0,0,0,0)", foreground: "#1b1d24", cursor: "#4450cf", cursorAccent: "#ffffff",
  selectionBackground: "rgba(68,80,207,0.25)",
  black: "#1b1d24", red: "#c03535", green: "#127a4a", yellow: "#8a5a00", blue: "#2f5fd0", magenta: "#7b45c9", cyan: "#0f7c86", white: "#5f6577",
  brightBlack: "#4f5566", brightRed: "#d64545", brightGreen: "#1a9a5e", brightYellow: "#a86f00", brightBlue: "#4a74e0", brightMagenta: "#9359dd", brightCyan: "#1597a2", brightWhite: "#1b1d24",
};

/** 读取页面当前主题，返回对应终端配色 */
export function currentTerminalTheme(): ITheme {
  return document.documentElement.dataset.theme === "light" ? light : dark;
}
