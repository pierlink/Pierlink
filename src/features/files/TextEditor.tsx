import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { Save, X } from "lucide-react";
import { Button, Dialog } from "../../components";
import { formatError } from "../../errors";
import { getFilesApi } from "./api";
import { baseName } from "./path";
import type { FileVersion, Loc } from "./types";
import "./editor.css";

/** 原文件的换行方式：CRLF 占多数就按 CRLF 保存 */
function lineEnding(text: string): "crlf" | "lf" {
  const crlf = (text.match(/\r\n/g) ?? []).length;
  const lf = (text.match(/\n/g) ?? []).length - crlf;
  return crlf > lf ? "crlf" : "lf";
}

/** 缩进用什么：有行以 Tab 开头就用 Tab，否则按最常见的空格缩进（默认 4 个） */
function indentUnit(text: string): string {
  const lines = text.split("\n").slice(0, 2000);
  if (lines.some((l) => l.startsWith("\t"))) return "\t";
  const widths = lines.map((l) => /^( +)\S/.exec(l)?.[1].length ?? 0).filter((n) => n > 0);
  const min = widths.length ? Math.min(...widths) : 4;
  return " ".repeat(min === 2 ? 2 : 4);
}

/**
 * 软件内的文本编辑器（简单版）：打开本机或服务器上的文本文件，Ctrl+S 保存。
 * - 保留原文件的换行方式（CRLF/LF）和 UTF-8 BOM；
 * - 保存前检查文件在打开后有没有被别人改过，改过时让用户选择覆盖或重新载入；
 * - 有没保存的修改时关闭要确认。
 */
export function TextEditor({ loc, path, where, onClose, onSaved }: {
  loc: Loc;
  path: string;
  /** 所在机器的显示名（本机 / 服务器名） */
  where: string;
  onClose: () => void;
  /** 保存成功（所在目录的列表需要刷新） */
  onSaved: () => void;
}) {
  const { t } = useTranslation();
  const [state, setState] = useState<"loading" | "ready" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [saved, setSaved] = useState("");
  const [saving, setSaving] = useState(false);
  const [ask, setAsk] = useState<null | "changed" | "close">(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [cursor, setCursor] = useState({ line: 1, col: 1 });
  const meta = useRef<{ bom: boolean; eol: "crlf" | "lf"; version: FileVersion | null; indent: string }>({ bom: false, eol: "lf", version: null, indent: "    " });
  const area = useRef<HTMLTextAreaElement>(null);
  const gutter = useRef<HTMLPreElement>(null);
  const dirty = state === "ready" && text !== saved;

  const load = async () => {
    setState("loading");
    try {
      const f = await (await getFilesApi()).readText(loc, path);
      const eol = lineEnding(f.text);
      const body = f.text.replace(/\r\n/g, "\n");
      meta.current = { bom: f.bom, eol, version: f.version, indent: indentUnit(body) };
      setText(body);
      setSaved(body);
      setState("ready");
      setError(null);
    } catch (e) {
      setError(formatError(e));
      setState("error");
    }
  };
  useEffect(() => { void load(); }, [loc, path]); // eslint-disable-line react-hooks/exhaustive-deps
  useLayoutEffect(() => { if (state === "ready") area.current?.focus(); }, [state]);

  /** 保存；force 为 true 时不检查是否被别人改过（用户选了覆盖） */
  const save = async (force = false): Promise<boolean> => {
    if (state !== "ready" || saving) return false;
    setSaving(true);
    try {
      const body = meta.current.eol === "crlf" ? text.replace(/\n/g, "\r\n") : text;
      const version = await (await getFilesApi()).writeText(loc, path, body, meta.current.bom, force ? null : meta.current.version);
      meta.current.version = version;
      setSaved(text);
      setNotice(t("editor.saved"));
      onSaved();
      return true;
    } catch (e) {
      if ((e as { code?: string } | null)?.code === "files.changed") setAsk("changed");
      else setNotice(formatError(e));
      return false;
    } finally {
      setSaving(false);
    }
  };
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), 3000);
    return () => clearTimeout(timer);
  }, [notice]);

  const close = () => { if (dirty) setAsk("close"); else onClose(); };

  const onKey = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") { e.preventDefault(); void save(); return; }
    if (e.key === "Tab" && !e.ctrlKey && !e.altKey) {
      // Tab 插入缩进（不把焦点移走）；用 insertText 让撤销（Ctrl+Z）照常可用
      e.preventDefault();
      if (!e.shiftKey) document.execCommand("insertText", false, meta.current.indent);
      return;
    }
  };
  const updateCursor = () => {
    const el = area.current;
    if (!el) return;
    const before = el.value.slice(0, el.selectionStart);
    const line = before.split("\n").length;
    setCursor({ line, col: el.selectionStart - before.lastIndexOf("\n") });
  };
  // 窗口级快捷键：Esc 关闭（有子对话框时交给子对话框）
  useEffect(() => {
    const onWin = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !ask) { e.preventDefault(); close(); }
    };
    window.addEventListener("keydown", onWin);
    return () => window.removeEventListener("keydown", onWin);
  });

  const lines = state === "ready" ? text.split("\n").length : 0;
  const numbers = Array.from({ length: lines }, (_, i) => i + 1).join("\n");

  return createPortal(
    <div className="pl-overlay editor-overlay">
      <div className="editor" role="dialog" aria-modal="true" aria-label={t("editor.title", { name: baseName(path) })}>
        <div className="editor__head">
          <div className="editor__name">
            <span className="editor__file">{baseName(path)}</span>
            {dirty && <span className="editor__dot" title={t("editor.unsaved")} aria-label={t("editor.unsaved")} />}
            <span className="editor__path mono" title={path}>{where} · {path}</span>
          </div>
          <Button variant="primary" size="sm" icon={<Save size={13} />} disabled={!dirty || saving} onClick={() => void save()}>
            {saving ? t("editor.saving") : t("editor.save")}
          </Button>
          <button type="button" className="pane__btn" title={t("common.close")} aria-label={t("common.close")} onClick={close}><X size={15} /></button>
        </div>
        <div className="editor__body">
          {state === "loading" && <div className="editor__msg" role="status">{t("files.loading")}</div>}
          {state === "error" && (
            <div className="editor__msg editor__msg--bad" role="alert">
              {error}
              <div style={{ marginTop: 12 }}><Button onClick={onClose}>{t("common.close")}</Button></div>
            </div>
          )}
          {state === "ready" && (
            <>
              <pre ref={gutter} className="editor__gutter mono" aria-hidden="true">{numbers}</pre>
              <textarea
                ref={area}
                className="editor__text mono"
                spellCheck={false}
                autoCapitalize="off"
                autoCorrect="off"
                wrap="off"
                value={text}
                aria-label={baseName(path)}
                onChange={(e) => { setText(e.target.value); updateCursor(); }}
                onKeyDown={onKey}
                onKeyUp={updateCursor}
                onClick={updateCursor}
                onScroll={(e) => { if (gutter.current) gutter.current.scrollTop = e.currentTarget.scrollTop; }}
              />
            </>
          )}
        </div>
        <div className="editor__foot">
          <span>{notice ?? t("editor.hint")}</span>
          <span className="editor__status mono">
            {state === "ready" && `${t("editor.position", cursor)} · UTF-8${meta.current.bom ? " BOM" : ""} · ${meta.current.eol.toUpperCase()}`}
          </span>
        </div>
      </div>
      {ask === "changed" && (
        <Dialog
          title={t("editor.changedTitle")}
          danger
          onDismiss={() => setAsk(null)}
          actions={<>
            <Button onClick={() => setAsk(null)}>{t("common.cancel")}</Button>
            <Button onClick={() => { setAsk(null); void load(); }}>{t("editor.reload")}</Button>
            <Button variant="danger" onClick={() => { setAsk(null); void save(true); }}>{t("editor.overwrite")}</Button>
          </>}
        >
          {t("editor.changedBody")}
        </Dialog>
      )}
      {ask === "close" && (
        <Dialog
          title={t("editor.closeTitle", { name: baseName(path) })}
          onDismiss={() => setAsk(null)}
          actions={<>
            <Button onClick={() => setAsk(null)}>{t("common.cancel")}</Button>
            <Button variant="danger" onClick={() => { setAsk(null); onClose(); }}>{t("editor.discard")}</Button>
            <Button variant="primary" onClick={() => { setAsk(null); void save().then((ok) => { if (ok) onClose(); }); }}>{t("editor.saveClose")}</Button>
          </>}
        >
          {t("editor.closeBody")}
        </Dialog>
      )}
    </div>,
    document.body,
  );
}
