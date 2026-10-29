import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Badge, Banner, Button, Dialog, TextField } from "../../components";
import { formatError } from "../../errors";
import { getImportApi, type ImportCandidate, type ImportPreview } from "./importApi";
import "./servers.css";

/** 显示用的连接参数：用户@主机[:端口] */
function target(c: ImportCandidate): string {
  const port = c.port && c.port !== 22 ? `:${c.port}` : "";
  return `${c.user ? `${c.user}@` : ""}${c.hostname ?? ""}${port}`;
}

const selectable = (c: ImportCandidate) => !c.alreadyImported && c.error === null;

/**
 * 从 ~/.ssh/config 导入服务器（设计“参数来源”：导入的服务器是链接型，参数以 ssh config 为准）。
 * 列出每个别名经 `ssh -G` 解析出的实际参数，标出跳板机、端口转发、关闭主机密钥校验等需要注意的设置。
 */
export function ImportDialog({ onImported, onClose }: { onImported: () => void; onClose: () => void }) {
  const { t } = useTranslation();
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [query, setQuery] = useState("");
  const [group, setGroup] = useState("ssh config");
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState<{ alias: string; error: unknown }[]>([]);
  const [showSkipped, setShowSkipped] = useState(false);

  useEffect(() => {
    // 每次挂载用自己的标志：开发模式下会“挂载→卸载→再挂载”，前一次（已被取消的）结果不能显示出来
    let alive = true;
    void getImportApi().then((api) => api.preview()).then(
      (p) => {
        if (!alive) return;
        setPreview(p);
        // 默认全选可导入的
        setPicked(new Set(p.candidates.filter(selectable).map((c) => c.alias)));
      },
      (e: unknown) => { if (alive) setLoadError(formatError(e)); },
    );
    return () => {
      alive = false;
      // 关闭时还在解析就取消，结束后台的 ssh 进程
      void getImportApi().then((api) => api.cancel());
    };
  }, []);

  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    return (preview?.candidates ?? []).filter((c) => !q || c.alias.toLowerCase().includes(q) || (c.hostname ?? "").toLowerCase().includes(q));
  }, [preview, query]);
  const shownSelectable = shown.filter(selectable);
  const allShownPicked = shownSelectable.length > 0 && shownSelectable.every((c) => picked.has(c.alias));

  const toggle = (alias: string, on: boolean) => setPicked((s) => { const n = new Set(s); if (on) n.add(alias); else n.delete(alias); return n; });
  const toggleAll = () => setPicked((s) => {
    const n = new Set(s);
    for (const c of shownSelectable) { if (allShownPicked) n.delete(c.alias); else n.add(c.alias); }
    return n;
  });

  const submit = async () => {
    setBusy(true);
    try {
      const aliases = (preview?.candidates ?? []).filter((c) => selectable(c) && picked.has(c.alias)).map((c) => c.alias);
      const fails = await (await getImportApi()).importAliases(aliases, group.trim() || null);
      onImported();
      if (fails.length === 0) { onClose(); return; }
      // 成功的标为已导入并取消勾选，再点“导入”时只剩真正失败的
      const failedSet = new Set(fails.map((f) => f.alias));
      const done = new Set(aliases.filter((a) => !failedSet.has(a)));
      setPreview((p) => p && { ...p, candidates: p.candidates.map((c) => (done.has(c.alias) ? { ...c, alreadyImported: true } : c)) });
      setPicked((s) => new Set([...s].filter((a) => !done.has(a))));
      setFailed(fails);
    } catch (e) {
      setFailed([{ alias: "", error: e }]);
    } finally {
      setBusy(false);
    }
  };

  const count = [...picked].filter((a) => preview?.candidates.some((c) => c.alias === a && selectable(c))).length;

  return (
    <Dialog
      title={t("import.title")}
      size="lg"
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="primary" disabled={busy || count === 0} onClick={() => void submit()}>
          {busy ? t("import.importing") : t("import.submit", { count })}
        </Button>
      </>}
    >
      <div className="imp">
        <p className="imp__lead">{t("import.lead")}</p>
        {loadError && <Banner tone="danger" title={t("import.loadFailed")}>{loadError}</Banner>}
        {!preview && !loadError && <span role="status" className="imp__muted">{t("import.reading")}</span>}
        {preview && (
          <>
            <div className="imp__bar">
              <span className="imp__muted mono" title={preview.configPath}>{preview.configPath}</span>
              <input
                type="search"
                className="pl-input imp__search"
                aria-label={t("common.search")}
                placeholder={t("import.search")}
                value={query}
                onChange={(e) => setQuery(e.target.value)}
              />
            </div>
            {preview.warnings.map((w, i) => <Banner key={i} tone="warn" title={t("import.warning")}>{formatError(w)}</Banner>)}
            {preview.candidates.length === 0 ? (
              <span className="imp__muted">{t("import.empty")}</span>
            ) : (
              <div className="imp__list" role="group" aria-label={t("import.title")}>
                <label className="imp__row imp__row--head">
                  <input type="checkbox" checked={allShownPicked} disabled={shownSelectable.length === 0} onChange={toggleAll} />
                  <span>{t("import.selectAll")}</span>
                </label>
                {shown.map((c) => (
                  <label key={c.alias} className={`imp__row${selectable(c) ? "" : " imp__row--off"}`}>
                    <input type="checkbox" checked={selectable(c) && picked.has(c.alias)} disabled={!selectable(c)} onChange={(e) => toggle(c.alias, e.target.checked)} />
                    <span className="imp__main">
                      <span className="imp__alias">{c.alias}</span>
                      <span className="imp__muted mono">
                        {c.error !== null ? formatError(c.error) : target(c)}
                      </span>
                    </span>
                    <span className="imp__tags">
                      {c.alreadyImported && <Badge>{t("import.already")}</Badge>}
                      {c.viaJump && <Badge tone="accent">{t("import.viaJump")}</Badge>}
                      {c.hasForwards && <span title={t("import.forwardsHint")}><Badge tone="warn">{t("import.forwards")}</Badge></span>}
                      {c.hostKeyCheckWeakened && <span title={t("import.weakHint")}><Badge tone="danger">{t("import.weak")}</Badge></span>}
                    </span>
                  </label>
                ))}
              </div>
            )}
            {preview.skipped.length > 0 && (
              <div className="imp__skipped">
                <button type="button" className="imp__link" onClick={() => setShowSkipped((v) => !v)} aria-expanded={showSkipped}>
                  {t("import.skipped", { count: preview.skipped.length })}
                </button>
                {showSkipped && (
                  <ul>
                    {preview.skipped.map((s, i) => (
                      <li key={i}><span className="mono">{s.pattern}</span> — {t(`import.skipReason.${s.reason}`)}</li>
                    ))}
                  </ul>
                )}
              </div>
            )}
            <TextField label={t("import.group")} value={group} placeholder={t("servers.groupPlaceholder")} onChange={(e) => setGroup(e.target.value)} />
            {failed.length > 0 && (
              <Banner tone="danger" title={t("import.failed")}>
                {failed.map((f) => `${f.alias ? `${f.alias}：` : ""}${formatError(f.error)}`).join("；")}
              </Banner>
            )}
          </>
        )}
      </div>
    </Dialog>
  );
}
