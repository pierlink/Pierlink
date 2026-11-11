import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { getFilesApi } from "./api";
import { dismissDelete, startDelete, stopDelete, useDeletes } from "./deletes";
import { baseName, locKey } from "./path";
import type { Loc } from "./types";

/**
 * 删除确认：本机放进回收站；服务器上永久删除（不能恢复），fast 时用 rm -rf 一次删完。
 * 点了删除确认框马上关掉，删除在后台进行，进度在文件面板下方的浮窗里（DeleteJobs）。
 */
export function DeleteDialog({ loc, paths, fast, onClose }: { loc: Loc; paths: string[]; fast: boolean; onClose: () => void }) {
  const { t } = useTranslation();
  const local = loc.kind === "local";
  const names = paths.map(baseName);
  const run = () => {
    onClose();
    void startDelete(loc, paths, fast);
  };
  return (
    <Dialog
      title={local ? t("files.deleteLocalTitle", { count: paths.length }) : t("files.deleteRemoteTitle", { count: paths.length })}
      danger
      onDismiss={onClose}
      actions={<>
        <Button onClick={onClose}>{t("common.cancel")}</Button>
        <Button variant="danger" onClick={run}>{local ? t("files.moveToTrash") : fast ? t("files.fastDelete") : t("files.deleteForever")}</Button>
      </>}
    >
      <p style={{ marginTop: 0 }}>{local ? t("files.deleteLocalBody") : fast ? t("files.fastDeleteBody") : t("files.deleteRemoteBody")}</p>
      <ul className="xfer-conflicts mono">
        {names.slice(0, 20).map((n) => <li key={n}>{n}</li>)}
        {names.length > 20 && <li>{t("files.andMore", { count: names.length - 20 })}</li>}
      </ul>
    </Dialog>
  );
}

/**
 * 文件面板下方的删除浮窗（不挡别的操作）：逐项删除时写已经删掉几项、正在删哪个，× 停下；
 * 快速删除、移到回收站只写“正在删除”；失败时写原因，× 关掉。
 */
export function DeleteJobs({ loc }: { loc: Loc }) {
  const { t } = useTranslation();
  const key = locKey(loc);
  const list = useDeletes().filter((j) => j.loc === key);
  if (!list.length) return null;
  return (
    <div className="files__deletes">
      {list.map((j) => {
        const failed = j.state === "failed";
        const text = failed ? j.error ?? ""
          : j.mode === "local" ? t("files.deletingLocal", { count: j.count })
            : j.mode === "fast" ? t("files.deletingFast", { count: j.count })
              : j.stopping ? t("files.deleteStopping") : t("files.deletingCount", { count: j.deleted });
        return (
          <div key={j.key} className={`files__delete${failed ? " files__delete--failed" : ""}`} role={failed ? "alert" : "status"}>
            <div className="files__delete-head">
              <span className="pl-ellipsis" title={text}>{text}</span>
              {(failed || j.mode === "normal") && (
                <button type="button" className="pane__btn" disabled={j.stopping}
                  title={failed ? t("common.close") : t("files.stopDelete")} aria-label={failed ? t("common.close") : t("files.stopDelete")}
                  onClick={() => (failed ? dismissDelete(j.key) : void stopDelete(j.key))}>
                  <X size={13} />
                </button>
              )}
            </div>
            {/* 正在删哪个：固定占一行，换文件时不跳 */}
            {!failed && j.mode === "normal" && <span className="files__delete-path mono pl-ellipsis" title={j.current ?? ""}>{j.current ?? ""}</span>}
          </div>
        );
      })}
    </div>
  );
}

const BITS: { who: "owner" | "group" | "others"; shift: number }[] = [
  { who: "owner", shift: 6 },
  { who: "group", shift: 3 },
  { who: "others", shift: 0 },
];
const PERMS: { what: "read" | "write" | "exec"; bit: number }[] = [
  { what: "read", bit: 4 },
  { what: "write", bit: 2 },
  { what: "exec", bit: 1 },
];

/** 修改服务器文件的权限：勾选或直接输入八进制数字（如 644、755） */
export function ChmodDialog({ loc, path, onClose, onDone }: { loc: Loc; path: string; onClose: () => void; onDone: (error: string | null) => void }) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<number | null>(null);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    void getFilesApi().then((api) => api.mode(loc, path)).then((m) => {
      const v = m ?? 0o644;
      setMode(v);
      setText(v.toString(8).padStart(3, "0"));
    }, (e: unknown) => setError(formatError(e)));
  }, [loc, path]);
  const setBits = (v: number) => { setMode(v); setText(v.toString(8).padStart(3, "0")); };
  const onText = (s: string) => {
    const clean = s.replace(/[^0-7]/g, "").slice(0, 4);
    setText(clean);
    if (clean.length >= 3) setMode(parseInt(clean, 8));
  };
  const run = async () => {
    if (mode === null) return;
    setBusy(true);
    try {
      await (await getFilesApi()).chmod(loc, path, mode);
      onDone(null);
    } catch (e) {
      setBusy(false);
      setError(formatError(e));
    }
  };
  return (
    <Dialog
      title={t("files.chmodTitle", { name: baseName(path) })}
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="primary" disabled={busy || mode === null || text.length < 3} onClick={() => void run()}>{t("common.save")}</Button>
      </>}
    >
      {mode === null && !error && <span role="status">{t("files.loading")}</span>}
      {mode !== null && (
        <div className="chmod">
          <div className="chmod__grid" role="group" aria-label={t("files.chmodTitle", { name: baseName(path) })}>
            <span />
            {PERMS.map((p) => <span key={p.what} className="chmod__head">{t(`files.perm.${p.what}`)}</span>)}
            {BITS.map((b) => (
              <div key={b.who} style={{ display: "contents" }}>
                <span className="chmod__who">{t(`files.perm.${b.who}`)}</span>
                {PERMS.map((p) => {
                  const bit = p.bit << b.shift;
                  return (
                    <label key={p.what} className="chmod__cell">
                      <input type="checkbox" aria-label={`${t(`files.perm.${b.who}`)} ${t(`files.perm.${p.what}`)}`} checked={(mode & bit) !== 0} onChange={(e) => setBits(e.target.checked ? mode | bit : mode & ~bit)} />
                    </label>
                  );
                })}
              </div>
            ))}
          </div>
          <label className="pl-field" style={{ maxWidth: 160 }}>
            {t("files.chmodOctal")}
            <input className="pl-input mono" value={text} inputMode="numeric" onChange={(e) => onText(e.target.value)} />
          </label>
        </div>
      )}
      {error && <div className="files__op-error" role="alert">{error}</div>}
    </Dialog>
  );
}
