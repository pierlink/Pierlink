import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { KeyRound } from "lucide-react";
import { Button, Dialog, TextField } from "../../components";
import { formatError } from "../../errors";
import { ImportKeyDialog } from "./ImportKeyDialog";
import { onServersChanged } from "./api";
import { getImportedKeysApi, onKeysChanged, type ImportedKey } from "./importedKeys";
import "./servers.css";

/** 正在做的操作（各自一个小框） */
type Action =
  | { kind: "import" }
  | { kind: "rename"; key: ImportedKey }
  | { kind: "passphrase"; key: ImportedKey }
  | { kind: "remove"; key: ImportedKey };

/** 私钥的密码状态：已保存 / 连接时问 / 不用输 / 丢了 */
function passState(k: ImportedKey): "saved" | "ask" | "none" | "lost" {
  if (k.userPassphrase) return k.passphraseSaved ? "saved" : "ask";
  return k.passphraseSaved ? "none" : "lost";
}

/**
 * 设置 → 私钥：导入过的私钥列表（加密存在 Pierlink 里），可以导入、改名、保存或不再保存密码、删除。
 * 服务器的“指定私钥”从这里的私钥里选。
 */
export function ImportedKeysPanel() {
  const { t } = useTranslation();
  const [keys, setKeys] = useState<ImportedKey[] | null>(null);
  const [action, setAction] = useState<Action | null>(null);
  /** 列表上方固定占一行的出错原因（读列表、保存失败时） */
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(() => {
    void getImportedKeysApi().then((api) => api.list()).then((list) => { setKeys(list); }, (e: unknown) => setError(formatError(e)));
  }, []);
  // 设置页一直挂着：私钥在别处导入、服务器改了用哪把私钥时都要重读（“在用”跟着变）
  useEffect(() => {
    load();
    const offKeys = onKeysChanged(load);
    const offServers = onServersChanged(load);
    return () => { offKeys(); offServers(); };
  }, [load]);

  const forget = async (k: ImportedKey) => {
    setError(null);
    try {
      await (await getImportedKeysApi()).passphrase(k.id, null);
      load();
    } catch (e) {
      setError(formatError(e));
    }
  };

  return (
    <div className="set-card key-list">
      <div className="key-list__head">
        <span className="set-row__hint">{t("privateKeys.intro")}</span>
        <Button size="sm" onClick={() => { setError(null); setAction({ kind: "import" }); }}>{t("privateKeys.import")}</Button>
      </div>
      <span className="key-list__err" role={error ? "alert" : undefined}>{error ?? ""}</span>
      {keys !== null && keys.length === 0 && <div className="key-list__empty">{t("privateKeys.empty")}</div>}
      {keys?.map((k) => {
        const state = passState(k);
        return (
          <div key={k.id} className="key-row">
            <KeyRound size={16} className="key-row__icon" aria-hidden />
            <span className="key-row__text">
              <span className="key-row__name pl-ellipsis" title={k.comment || k.name}>{k.name}</span>
              <span className="key-row__meta mono pl-ellipsis" title={k.fingerprint}>{k.algorithm} · {k.fingerprint}</span>
              <span className={`key-row__state${state === "lost" ? " key-row__state--bad" : ""}`}>
                {k.usedBy.length ? t("privateKeys.usedBy", { count: k.usedBy.length, names: k.usedBy.join("、") }) : t("privateKeys.unused")}
                {" · "}
                {t(`privateKeys.pass.${state}`)}
              </span>
            </span>
            <span className="key-row__actions">
              {state === "ask" && <Button size="sm" variant="ghost" onClick={() => { setError(null); setAction({ kind: "passphrase", key: k }); }}>{t("privateKeys.savePass")}</Button>}
              {state === "saved" && <Button size="sm" variant="ghost" onClick={() => void forget(k)}>{t("privateKeys.forgetPass")}</Button>}
              <Button size="sm" variant="ghost" onClick={() => { setError(null); setAction({ kind: "rename", key: k }); }}>{t("privateKeys.rename")}</Button>
              <Button size="sm" variant="ghost" onClick={() => { setError(null); setAction({ kind: "remove", key: k }); }}>{t("common.delete")}</Button>
            </span>
          </div>
        );
      })}

      {action?.kind === "import" && (
        <ImportKeyDialog onCancel={() => setAction(null)} onDone={() => { setAction(null); load(); }} />
      )}
      {action?.kind === "rename" && <RenameDialog k={action.key} onClose={(changed) => { setAction(null); if (changed) load(); }} />}
      {action?.kind === "passphrase" && <PassphraseDialog k={action.key} onClose={(changed) => { setAction(null); if (changed) load(); }} />}
      {action?.kind === "remove" && <RemoveDialog k={action.key} onClose={(changed) => { setAction(null); if (changed) load(); }} />}
    </div>
  );
}

/** 一个输入框 + 保存的小框：改名、保存密码共用 */
function InputDialog({ title, label, initial, password, onSubmit, onClose }: {
  title: string;
  label: string;
  initial: string;
  password?: boolean;
  onSubmit: (value: string) => Promise<void>;
  onClose: (changed: boolean) => void;
}) {
  const { t } = useTranslation();
  const [value, setValue] = useState(initial);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const submit = async () => {
    if (busy || !value.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await onSubmit(value);
      onClose(true);
    } catch (e) {
      setError(formatError(e));
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={title}
      onDismiss={busy ? undefined : () => onClose(false)}
      actions={(
        <>
          <Button onClick={() => onClose(false)} disabled={busy}>{t("common.cancel")}</Button>
          <Button variant="primary" onClick={() => void submit()} disabled={busy || !value.trim()}>{t("common.save")}</Button>
        </>
      )}
    >
      <form className="key-import" onSubmit={(e) => { e.preventDefault(); void submit(); }}>
        <TextField label={label} type={password ? "password" : "text"} value={value} onChange={(e) => { setValue(e.target.value); setError(null); }} autoFocus autoComplete="off" maxLength={password ? undefined : 60} aria-invalid={!!error} />
        <span className="key-import__err" role={error ? "alert" : undefined}>{error ?? ""}</span>
      </form>
    </Dialog>
  );
}

function RenameDialog({ k, onClose }: { k: ImportedKey; onClose: (changed: boolean) => void }) {
  const { t } = useTranslation();
  return (
    <InputDialog title={t("privateKeys.renameTitle")} label={t("privateKeys.name")} initial={k.name} onClose={onClose}
      onSubmit={async (name) => { await (await getImportedKeysApi()).rename(k.id, name); }} />
  );
}

function PassphraseDialog({ k, onClose }: { k: ImportedKey; onClose: (changed: boolean) => void }) {
  const { t } = useTranslation();
  return (
    <InputDialog title={t("privateKeys.savePassTitle", { name: k.name })} label={t("privateKeys.passphrase")} initial="" password onClose={onClose}
      onSubmit={async (p) => { await (await getImportedKeysApi()).passphrase(k.id, p); }} />
  );
}

function RemoveDialog({ k, onClose }: { k: ImportedKey; onClose: (changed: boolean) => void }) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const remove = async () => {
    setBusy(true);
    setError(null);
    try {
      await (await getImportedKeysApi()).remove(k.id);
      onClose(true);
    } catch (e) {
      setError(formatError(e));
      setBusy(false);
    }
  };
  return (
    <Dialog
      danger
      title={t("privateKeys.removeTitle", { name: k.name })}
      onDismiss={busy ? undefined : () => onClose(false)}
      actions={(
        <>
          <Button onClick={() => onClose(false)} disabled={busy}>{t("common.cancel")}</Button>
          <Button variant="danger" onClick={() => void remove()} disabled={busy}>{t("common.delete")}</Button>
        </>
      )}
    >
      <p>{t("privateKeys.removeBody")}</p>
      <p className="key-import__err key-import__err--block" role={error ? "alert" : undefined}>{error ?? ""}</p>
    </Dialog>
  );
}
