import i18n from "../../i18n";
import { sampleScreen, samplePrompt, screenshotMode } from "../../dev/screenshots";
import { inTauri } from "../shell/TitleBar";
import { createPreviewBackend } from "./previewBackend";
import { createTauriBackend, listLocalShellsFromBackend } from "./tauriBackend";
import type { TerminalBackend } from "./types";

/** 本机终端的展示信息 */
export interface LocalShellView {
  id: string;
  name: string;
  isDefault: boolean;
}

let backend: TerminalBackend | null = null;

/** 取得终端后端：在 Tauri 窗口里用真实后端，在浏览器里调试时用预览实现 */
export function getTerminalBackend(): TerminalBackend {
  backend ??= inTauri
    ? createTauriBackend()
    : screenshotMode
      ? createPreviewBackend(sampleScreen, samplePrompt)
      : createPreviewBackend(() => i18n.t("terminal.previewBanner"));
  return backend;
}

/** 本机终端列表 */
export async function listLocalShells(): Promise<LocalShellView[]> {
  if (inTauri) return listLocalShellsFromBackend();
  return [
    { id: "pwsh", name: "PowerShell 7", isDefault: true },
    { id: "powershell", name: "Windows PowerShell 5.1", isDefault: false },
    { id: "gitbash", name: "Git Bash", isDefault: false },
    { id: "cmd", name: "命令提示符 / Command Prompt", isDefault: false },
  ];
}
