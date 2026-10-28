import { useEffect, useState } from "react";
import { commands } from "../../bindings/commands";
import { inTauri } from "./TitleBar";

/** 应用版本与平台（在 Tauri 里向后端查询；浏览器预览时为 null） */
export function useAppVersion(): string | null {
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    if (!inTauri) return;
    commands.appInfo().then((i) => setVersion(`${i.version} · ${i.os} ${i.arch}`), () => setVersion(null));
  }, []);
  return version;
}
