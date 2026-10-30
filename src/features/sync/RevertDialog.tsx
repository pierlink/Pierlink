import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Dialog } from "../../components";
import { formatError } from "../../errors";

/**
 * 还原本机改动前的确认（会删文件）：本机新增的项会被删除，改过的会被覆盖成服务器上的版本。
 * 开着旧版本保留时被替换的文件先放进旧版本；关着时无法找回，要写清楚。
 */
export function RevertDialog({ count, versioning, onConfirm, onClose }: {
  count: number;
  versioning: boolean;
  onConfirm: () => Promise<void>;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      await onConfirm();
      onClose();
    } catch (e) {
      setError(formatError(e));
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={t("sync.revertTitle", { count })}
      danger
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="danger" disabled={busy} onClick={() => void run()}>{busy ? t("sync.reverting") : t("sync.revertLocal")}</Button>
      </>}
    >
      <p style={{ marginTop: 0 }}>{t("sync.revertBody")}</p>
      <p style={{ marginBottom: 0 }}><b>{versioning ? t("sync.revertKept") : t("sync.revertLost")}</b></p>
      {error && <div style={{ marginTop: 10 }}><Banner tone="danger" title={t("sync.revertFailed")}>{error}</Banner></div>}
    </Dialog>
  );
}
