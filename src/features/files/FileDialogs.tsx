import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { getFilesApi } from "./api";
import { baseName } from "./path";
import type { Loc } from "./types";

/** 删除确认：本机放进回收站；服务器上永久删除（不能恢复） */
export function DeleteDialog({ loc, paths, onClose, onDone }: { loc: Loc; paths: string[]; onClose: () => void; onDone: (error: string | null) => void }) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const local = loc.kind === "local";
  const names = paths.map(baseName);
  const run = async () => {
    setBusy(true);
    try {
      await (await getFilesApi()).remove(loc, paths);
      onDone(null);
    } catch (e) {
      onDone(formatError(e));
    }
  };
  return (
    <Dialog
      title={local ? t("files.deleteLocalTitle", { count: paths.length }) : t("files.deleteRemoteTitle", { count: paths.length })}
      danger
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="danger" disabled={busy} onClick={() => void run()}>{local ? t("files.moveToTrash") : t("files.deleteForever")}</Button>
      </>}
    >
      <p style={{ marginTop: 0 }}>{local ? t("files.deleteLocalBody") : t("files.deleteRemoteBody")}</p>
      <ul className="xfer-conflicts mono">
        {names.slice(0, 20).map((n) => <li key={n}>{n}</li>)}
        {names.length > 20 && <li>{t("files.andMore", { count: names.length - 20 })}</li>}
      </ul>
    </Dialog>
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
