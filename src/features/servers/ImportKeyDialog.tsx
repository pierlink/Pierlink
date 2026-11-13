import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Button, Checkbox, Dialog, TextField } from "../../components";
import { formatError, isAppError } from "../../errors";
import { getImportedKeysApi, type ImportedKey } from "./importedKeys";

/**
 * 导入私钥：选一次私钥文件，Pierlink 加密后存起来（原文件不动）。
 * 私钥有密码时填上密码，可以选择保存（存进 Windows 凭据管理器，连接时自动填）。
 * @param initialPath 预先填好的私钥文件（把老服务器按路径用的私钥导入时）
 * @param onDone 导入好以后（带着导入的私钥）
 * @param onCancel 关掉
 */
export function ImportKeyDialog({ initialPath = "", onDone, onCancel }: { initialPath?: string; onDone: (key: ImportedKey) => void; onCancel: () => void }) {
  const { t } = useTranslation();
  const [path, setPath] = useState(initialPath);
  const [name, setName] = useState("");
  const [passphrase, setPassphrase] = useState("");
  const [save, setSave] = useState(true);
  const [busy, setBusy] = useState(false);
  /** 出错的是哪一栏：密码不对、要密码时写在密码栏下面，其他写在底部 */
  const [error, setError] = useState<{ field: "passphrase" | "other"; text: string } | null>(null);

  const pick = async () => {
    setError(null);
    try {
      const picked = await (await getImportedKeysApi()).pickFile();
      if (picked) setPath(picked);
    } catch (e) {
      setError({ field: "other", text: formatError(e) });
    }
  };
  /** 资源管理器“复制为路径”带着引号：去掉 */
  const cleanPath = (p: string) => p.trim().replace(/^"(.*)"$/, "$1").trim();
  const submit = async () => {
    if (!cleanPath(path) || busy) return;
    setBusy(true);
    setError(null);
    try {
      const key = await (await getImportedKeysApi()).import(cleanPath(path), name, passphrase || null, save);
      onDone(key);
    } catch (e) {
      const onPassphrase = isAppError(e) && (e.code === "keys.imported_needs_passphrase" || e.code === "keys.imported_wrong_passphrase");
      setError({ field: onPassphrase ? "passphrase" : "other", text: formatError(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      title={t("privateKeys.importTitle")}
      onDismiss={busy ? undefined : onCancel}
      actions={(
        <>
          <Button onClick={onCancel} disabled={busy}>{t("common.cancel")}</Button>
          <Button variant="primary" onClick={() => void submit()} disabled={busy || !cleanPath(path)}>{busy ? t("privateKeys.importing") : t("privateKeys.doImport")}</Button>
        </>
      )}
    >
      <form className="key-import" onSubmit={(e) => { e.preventDefault(); void submit(); }}>
        <div className="key-import__file">
          <TextField label={t("privateKeys.file")} className="pl-input mono" value={path} onChange={(e) => { setPath(e.target.value); setError(null); }} autoFocus={!initialPath} spellCheck={false} />
          <Button onClick={() => void pick()} disabled={busy}>{t("privateKeys.pick")}</Button>
        </div>
        <TextField label={t("privateKeys.name")} placeholder={t("privateKeys.namePlaceholder")} value={name} onChange={(e) => setName(e.target.value)} maxLength={60} />
        <TextField
          label={t("privateKeys.passphrase")}
          type="password"
          placeholder={t("privateKeys.passphraseHint")}
          value={passphrase}
          onChange={(e) => { setPassphrase(e.target.value); if (error?.field === "passphrase") setError(null); }}
          autoFocus={!!initialPath}
          autoComplete="off"
          aria-invalid={error?.field === "passphrase"}
        />
        {/* 出错的原因固定占一行，出现、消失时框不跳 */}
        <span className="key-import__err" role={error?.field === "passphrase" ? "alert" : undefined}>{error?.field === "passphrase" ? error.text : ""}</span>
        <Checkbox label={t("privateKeys.savePassphrase")} checked={save && !!passphrase} disabled={!passphrase} onChange={(e) => setSave(e.target.checked)} />
        <p className="key-import__hint">{t("privateKeys.importHint")}</p>
        <p className="key-import__err key-import__err--block" role={error?.field === "other" ? "alert" : undefined}>{error?.field === "other" ? error.text : ""}</p>
        {/* 让回车提交 */}
        <button type="submit" hidden />
      </form>
    </Dialog>
  );
}
