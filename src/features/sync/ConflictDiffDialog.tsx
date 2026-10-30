import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Banner, Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { formatBytes } from "./format";
import { getSyncApi, type ConflictDiff, type DiffFile } from "./syncApi";
import "./sync.css";

function when(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 去掉行尾换行（对比数据保留了原来的行尾） */
function line(text: string | null): string {
  return text === null ? "" : text.replace(/\r?\n$/, "");
}

/**
 * 冲突“对比两个版本”：左边当前版本，右边冲突副本。文本并排显示差异；
 * 二进制或太大的文件只显示两边的大小和修改时间。看完后在这里直接选保留哪个。
 */
export function ConflictDiffDialog({ taskId, path, conflictFile, onKeep, onClose }: {
  taskId: string;
  path: string;
  conflictFile: string;
  onKeep: (keep: "current" | "other") => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [diff, setDiff] = useState<ConflictDiff | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    void getSyncApi().then((api) => api.conflictDiff(taskId, path, conflictFile)).then(setDiff, (e: unknown) => setError(formatError(e)));
  }, [taskId, path, conflictFile]);

  const meta = (label: string, f: DiffFile) => (
    <div className="cdiff__meta">
      <b>{label}</b>
      <span>{formatBytes(f.size)} · {t("sync.diff.modified", { time: when(f.modifiedMs) })}{f.encoding ? ` · ${f.encoding}` : ""}</span>
    </div>
  );
  const changed = diff?.kind === "text" ? diff.lines.filter((l) => l.kind !== "equal").length : 0;

  return (
    <Dialog
      title={t("sync.diff.title", { path })}
      size="xl"
      onDismiss={onClose}
      actions={<>
        <Button onClick={onClose}>{t("deploy.close")}</Button>
        <Button onClick={() => onKeep("current")}>{t("sync.conflictKeep")}</Button>
        <Button variant="primary" onClick={() => onKeep("other")}>{t("sync.conflictUseOther")}</Button>
      </>}
    >
      <div className="cdiff">
        {error && <Banner tone="danger" title={t("sync.diff.failed")}>{error}</Banner>}
        {!diff && !error && <span role="status" className="sync-muted">{t("common.loading")}</span>}
        {diff && (
          <div className="cdiff__heads">
            {meta(t("sync.diff.current"), diff.left)}
            {meta(t("sync.diff.other"), diff.right)}
          </div>
        )}
        {diff?.kind === "binary" && <p className="sync-muted">{t("sync.diff.binary")}</p>}
        {diff?.kind === "tooLarge" && <p className="sync-muted">{t("sync.diff.tooLarge", { limit: formatBytes(diff.limit) })}</p>}
        {diff?.kind === "text" && (
          <>
            <p className="sync-muted">{changed === 0 ? t("sync.diff.same") : t("sync.diff.changed", { count: changed })}</p>
            <div className="cdiff__table" role="table" aria-label={t("sync.diff.title", { path })}>
              {diff.lines.map((l, i) => (
                <div key={i} className={`cdiff__row cdiff__row--${l.kind}`} role="row">
                  <span className="cdiff__num" role="cell">{l.leftNumber ?? ""}</span>
                  <span className="cdiff__text cdiff__text--left mono" role="cell">{line(l.left)}</span>
                  <span className="cdiff__num" role="cell">{l.rightNumber ?? ""}</span>
                  <span className="cdiff__text cdiff__text--right mono" role="cell">{line(l.right)}</span>
                </div>
              ))}
            </div>
          </>
        )}
      </div>
    </Dialog>
  );
}
