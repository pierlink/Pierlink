import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, Copy, FolderOpen, Plus, Trash2 } from "lucide-react";
import { Button, Checkbox, Select, TextField } from "../../components";
import type { ServerRecord } from "../servers/api";
import { getMcpApi, type EntrySettings, type McpApproval } from "./api";
import { addFolder, removeFolder, toggleServer } from "./model";
import "./mcp.css";
import { writeClipboardText } from "../shell/clipboard";

/** 一段可复制的命令 */
export function CommandLine({ text }: { text: string }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const copy = () => void writeClipboardText(text).then(() => { setCopied(true); setTimeout(() => setCopied(false), 1500); });
  return (
    <div className="mcp-cmd">
      <code className="mono">{text}</code>
      <button type="button" className="mcp-cmd__copy" aria-label={t("mcp.copy")} title={copied ? t("mcp.copied") : t("mcp.copy")} onClick={copy}>{copied ? <Check size={13} /> : <Copy size={13} />}</button>
    </div>
  );
}

/** 端口：离开输入框或按回车时才保存（每敲一个键就保存会反复重启转发）；不合法时恢复原值 */
export function PortInput({ value, label, onCommit }: { value: number; label: string; onCommit: (port: number) => void }) {
  const { t } = useTranslation();
  const [text, setText] = useState(String(value));
  const [bad, setBad] = useState(false);
  useEffect(() => setText(String(value)), [value]);
  const port = Number(text);
  const changed = text !== String(value);
  // 改端口会重开入口：点“应用”或按 Enter 才改（离开输入框不改），Esc 恢复
  const commit = () => {
    if (Number.isInteger(port) && port >= 1024 && port <= 65535) {
      setBad(false);
      if (port !== value) onCommit(port);
    } else {
      setBad(true);
    }
  };
  return (
    <>
      <TextField label={label} className="pl-input mono" inputMode="numeric" value={text} aria-invalid={bad} title={bad ? t("mcp.portInvalid") : undefined}
        onChange={(e) => { setBad(false); setText(e.target.value.replace(/[^0-9]/g, "").slice(0, 5)); }}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
          if (e.key === "Escape") { setBad(false); setText(String(value)); }
        }} />
      <Button size="sm" variant={changed ? "primary" : "secondary"} disabled={!changed} onClick={commit}>{t("mcp.portApply")}</Button>
      {/* 端口不对时文字放在固定宽度的位置，不把旁边的按钮挤开 */}
      <span className="mcp-port__err" role={bad ? "alert" : undefined}>{bad ? t("mcp.portInvalid") : ""}</span>
    </>
  );
}

/**
 * 一个入口的权限。服务器入口（AI 在服务器上）先列这台 Windows 的命令和文件夹，再列其他服务器，
 * 另有“每次连接都问”；本机入口（AI 在这台电脑上）主要用来操作服务器，服务器放最前面。
 */
export function EntryPermissions({ entry, servers, exclude, server, onChange }: {
  entry: EntrySettings;
  servers: ServerRecord[];
  /** 不列出的服务器（服务器入口自己） */
  exclude?: string;
  /** 服务器入口 */
  server: boolean;
  onChange: (e: EntrySettings) => void;
}) {
  const { t } = useTranslation();
  const [folder, setFolder] = useState("");
  const others = servers.filter((s) => s.id !== exclude);
  // 服务器入口：写明是哪台服务器上的 AI（以前只写“可以操作的其他服务器”，看不出是谁去操作）
  const self = servers.find((s) => s.id === exclude)?.name;
  const serverList = (
    <>
      <div className="mcp-perms__label">{server && self ? t("mcp.otherServers", { name: self }) : t("mcp.servers")}</div>
      <span className="set-row__hint">{others.length === 0 ? t("mcp.noServers") : t("mcp.serversHint")}</span>
      <div className="mcp-perms__servers">
        {others.map((s) => (
          <Checkbox key={s.id} label={s.name} checked={entry.servers.includes(s.id)} onChange={() => onChange(toggleServer(entry, s.id))} />
        ))}
      </div>
    </>
  );
  return (
    <div className="mcp-perms">
      <div className="mcp-perms__label">{t("mcp.approval")}</div>
      <Select label={t("mcp.approval")} value={entry.approval} onChange={(v) => onChange({ ...entry, approval: v as McpApproval })} options={[
        { value: "ask_all", label: t("mcp.approvalAskAll") },
        { value: "dangerous", label: t("mcp.approvalDangerous") },
        { value: "auto", label: t("mcp.approvalAuto") },
      ]} />
      <span className={`set-row__hint${entry.approval === "auto" ? " set-row__hint--warn" : ""}`}>{t(`mcp.approvalHint.${entry.approval}`)}</span>
      {server && (
        <>
          <Checkbox label={t("mcp.confirmConnections")} checked={entry.confirmConnections} onChange={(e) => onChange({ ...entry, confirmConnections: e.target.checked })} />
          <span className="set-row__hint">{t("mcp.confirmConnectionsHint")}</span>
        </>
      )}
      {!server && serverList}
      <Checkbox label={t("mcp.windowsCommands")} checked={entry.windowsCommands} onChange={(e) => onChange({ ...entry, windowsCommands: e.target.checked })} />
      <span className={`set-row__hint${server && entry.windowsCommands ? " set-row__hint--warn" : ""}`}>{t("mcp.windowsCommandsHint")}</span>
      <div className="mcp-perms__label">{t("mcp.folders")}</div>
      <span className="set-row__hint">{t("mcp.foldersHint")}</span>
      {entry.folders.map((f) => (
        <div key={f} className="mcp-perms__folder">
          <span className="mono pl-ellipsis">{f}</span>
          <button type="button" className="mcp-cmd__copy" aria-label={t("mcp.removeFolder")} title={t("mcp.removeFolder")} onClick={() => onChange(removeFolder(entry, f))}><Trash2 size={13} /></button>
        </div>
      ))}
      <div className="mcp-perms__add">
        <TextField label={t("mcp.folderPath")} className="pl-input mono" placeholder={t("mcp.folderPlaceholder")} value={folder} onChange={(e) => setFolder(e.target.value)} />
        <Button size="sm" icon={<Plus size={13} />} disabled={!folder.trim()} onClick={() => { onChange(addFolder(entry, folder)); setFolder(""); }}>{t("mcp.addFolder")}</Button>
        <Button size="sm" icon={<FolderOpen size={13} />} onClick={() => void getMcpApi().then((api) => api.pickFolder()).then((p) => { if (p) onChange(addFolder(entry, p)); }, () => {})}>{t("mcp.pickFolder")}</Button>
      </div>
      {server && serverList}
    </div>
  );
}
