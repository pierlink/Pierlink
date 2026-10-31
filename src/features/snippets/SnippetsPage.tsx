import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ClipboardPaste, Plus, Search, Send } from "lucide-react";
import { Badge, Banner, Button, Dialog, SelectField, TextField, useContextMenu } from "../../components";
import { formatError } from "../../errors";
import { getSnippetsApi, type SnippetRecord } from "./api";
import { parseVariables, prepareForSend, render } from "./variables";
import "./snippets.css";
import { writeClipboardText } from "../shell/clipboard";

/** 发送一条片段：text 为替换变量后的命令；execute 为真时再按一次回车 */
export type SendSnippet = (text: string, execute: boolean) => void;

/**
 * 工具面板“片段”页：按当前服务器所在分组、通用、其他分组列出命令片段。
 * “粘贴到终端”和“发送并执行”分开；含 {{变量}} 的先弹框填写并预览最终命令。
 * @param group 当前服务器所在分组（没有分组或没有服务器时为 null）
 * @param groups 已有的全部分组名（编辑片段时选择）
 * @param targets 片段现在会发到几个窗格（广播中为参与广播的窗格数；0 表示没有可用的终端）
 */
export function SnippetsPage({ group, groups, targets, onSend }: { group: string | null; groups: string[]; targets: number; onSend: SendSnippet }) {
  const { t } = useTranslation();
  const [items, setItems] = useState<SnippetRecord[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [editing, setEditing] = useState<SnippetRecord | "new" | null>(null);
  const [filling, setFilling] = useState<{ s: SnippetRecord; execute: boolean } | null>(null);
  const [deleting, setDeleting] = useState<SnippetRecord | null>(null);
  const [copied, setCopied] = useState(false);
  const menu = useContextMenu(t("snippets.menu"));

  const load = () => getSnippetsApi().then((api) => api.list()).then(setItems, (e: unknown) => setError(formatError(e)));
  useEffect(() => { void load(); }, []);

  // 分节：当前分组专用、通用、其他分组（包括分组已被改名或删除的）
  const sections = useMemo(() => {
    const q = query.trim().toLowerCase();
    const hit = (s: SnippetRecord) => !q || s.title.toLowerCase().includes(q) || s.body.toLowerCase().includes(q);
    const list = items.filter(hit).sort((a, b) => a.sortOrder - b.sortOrder || a.title.localeCompare(b.title));
    const mine = group ? list.filter((s) => s.groupName === group) : [];
    const common = list.filter((s) => !s.groupName);
    const other = list.filter((s) => s.groupName && s.groupName !== group);
    return [
      ...(group ? [{ key: "mine", title: t("snippets.groupOnly", { group }), items: mine }] : []),
      { key: "common", title: t("snippets.common"), items: common },
      { key: "other", title: t("snippets.otherGroups"), items: other },
    ].filter((x) => x.items.length > 0);
  }, [items, query, group, t]);

  const send = (s: SnippetRecord, execute: boolean) => {
    // 有变量要先填；广播中即使没有变量也先确认（会同时进入多个窗格）
    if (parseVariables(s.body).length > 0 || targets > 1) { setFilling({ s, execute }); return; }
    onSend(prepareForSend(s.body), execute);
  };
  const copy = (text: string) => void writeClipboardText(text).then(() => { setCopied(true); setTimeout(() => setCopied(false), 1500); });

  return (
    <div className="snip">
      <label className="srv-search">
        <Search size={13} aria-hidden="true" />
        <input value={query} onChange={(e) => setQuery(e.target.value)} placeholder={t("snippets.search")} aria-label={t("snippets.search")} />
      </label>
      {error && <Banner tone="danger" title={t("snippets.loadFailed")}>{error}</Banner>}
      {/* 发到哪里：一直占一行，切换标签、开始或停止广播时下面的片段不跳 */}
      <p className={`snip__hint snip__target pl-ellipsis${targets > 1 ? " snip__hint--warn" : ""}`}>
        {targets === 0 ? t("snippets.noTerminal") : targets > 1 ? t("snippets.broadcastTargets", { count: targets }) : t("snippets.oneTarget")}
      </p>
      {sections.length === 0 && !error && <p className="snip__hint">{query ? t("snippets.noMatch") : t("snippets.empty")}</p>}
      {sections.map((sec) => (
        <section key={sec.key} className="snip__section" aria-label={sec.title}>
          <h3 className="ws__label">{sec.title}</h3>
          {sec.items.map((s) => (
            <article
              key={s.id}
              className="snip__item"
              onContextMenu={(e) => menu.open(e, [
                { id: "paste", label: t("snippets.paste"), disabled: targets === 0, onSelect: () => send(s, false) },
                { id: "run", label: t("snippets.run"), disabled: targets === 0, onSelect: () => send(s, true) },
                { kind: "separator", id: "s1" },
                { id: "edit", label: t("snippets.edit"), onSelect: () => setEditing(s) },
                { id: "copy", label: t("snippets.copy"), onSelect: () => copy(s.body) },
                { kind: "separator", id: "s2" },
                { id: "delete", label: t("common.delete"), danger: true, onSelect: () => setDeleting(s) },
              ])}
            >
              <div className="snip__title">
                <span className="pl-ellipsis">{s.title}</span>
                {parseVariables(s.body).length > 0 && <Badge>{t("snippets.hasVariables")}</Badge>}
              </div>
              <pre className="snip__body mono">{s.body}</pre>
              <div className="snip__actions">
                <Button size="sm" icon={<ClipboardPaste size={13} />} disabled={targets === 0} onClick={() => send(s, false)}>{t("snippets.paste")}</Button>
                <Button size="sm" variant="primary" icon={<Send size={13} />} disabled={targets === 0} onClick={() => send(s, true)}>{t("snippets.run")}</Button>
              </div>
            </article>
          ))}
        </section>
      ))}
      <Button icon={<Plus size={13} />} onClick={() => setEditing("new")}>{t("snippets.new")}</Button>
      {menu.node}
      {copied && <div className="ws-toast" role="status">{t("snippets.copied")}</div>}

      {editing && (
        <SnippetDialog
          snippet={editing === "new" ? null : editing}
          defaultGroup={group}
          groups={groups}
          onSaved={() => { setEditing(null); void load(); }}
          onClose={() => setEditing(null)}
        />
      )}
      {filling && (
        <VariablesDialog
          snippet={filling.s}
          execute={filling.execute}
          targets={targets}
          onSend={(text, execute) => { setFilling(null); onSend(text, execute); }}
          onClose={() => setFilling(null)}
        />
      )}
      {deleting && (
        <Dialog
          title={t("snippets.deleteTitle", { title: deleting.title })}
          danger
          onDismiss={() => setDeleting(null)}
          actions={<>
            <Button onClick={() => setDeleting(null)}>{t("common.cancel")}</Button>
            <Button variant="danger" onClick={() => {
              const id = deleting.id;
              setDeleting(null);
              getSnippetsApi().then((api) => api.remove(id)).then(load, (e: unknown) => setError(formatError(e)));
            }}>{t("common.delete")}</Button>
          </>}
        >
          <pre className="snip__body mono">{deleting.body}</pre>
        </Dialog>
      )}
    </div>
  );
}

/** 新建或修改片段 */
function SnippetDialog({ snippet, defaultGroup, groups, onSaved, onClose }: {
  snippet: SnippetRecord | null;
  defaultGroup: string | null;
  groups: string[];
  onSaved: () => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [title, setTitle] = useState(snippet?.title ?? "");
  const [body, setBody] = useState(snippet?.body ?? "");
  const [groupName, setGroupName] = useState<string>(snippet ? snippet.groupName ?? "" : defaultGroup ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // 片段可能属于一个已经不存在的分组：选项里保留它
  const options = [
    { value: "", label: t("snippets.common") },
    ...[...new Set([...groups, ...(groupName ? [groupName] : [])])].sort().map((g) => ({ value: g, label: t("snippets.groupOnly", { group: g }) })),
  ];
  const vars = parseVariables(body);
  const save = () => {
    setBusy(true);
    setError(null);
    getSnippetsApi()
      .then((api) => api.save({ id: snippet?.id ?? null, title, body, groupName: groupName || null, sortOrder: snippet?.sortOrder ?? 0 }))
      .then(onSaved, (e: unknown) => { setError(formatError(e)); setBusy(false); });
  };
  return (
    <Dialog
      title={snippet ? t("snippets.editTitle") : t("snippets.newTitle")}
      onDismiss={busy ? undefined : onClose}
      actions={<>
        <Button onClick={onClose} disabled={busy}>{t("common.cancel")}</Button>
        <Button variant="primary" onClick={save} disabled={busy || !title.trim() || !body.trim()}>{t("common.save")}</Button>
      </>}
    >
      <div className="snip-form">
        <TextField label={t("snippets.titleLabel")} value={title} maxLength={100} onChange={(e) => setTitle(e.target.value)} autoFocus />
        <label className="pl-field">
          {t("snippets.bodyLabel")}
          <textarea className="pl-input mono snip-form__body" rows={5} value={body} spellCheck={false} onChange={(e) => setBody(e.target.value)} placeholder={t("snippets.bodyPlaceholder", { example: "git switch {{branch}} && git pull" })} />
        </label>
        <p className="snip__hint">{vars.length ? t("snippets.variablesFound", { names: vars.join(t("common.listSep")) }) : t("snippets.variablesHint")}</p>
        <SelectField label={t("snippets.groupLabel")} value={groupName} onChange={setGroupName} options={options} />
        {error && <Banner tone="danger" title={t("snippets.saveFailed")}>{error}</Banner>}
      </div>
    </Dialog>
  );
}

/** 填写变量后发送：预览最终命令，写明会发到几个窗格 */
function VariablesDialog({ snippet, execute, targets, onSend, onClose }: {
  snippet: SnippetRecord;
  execute: boolean;
  targets: number;
  onSend: SendSnippet;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const names = parseVariables(snippet.body);
  const [values, setValues] = useState<Record<string, string>>(() => Object.fromEntries(names.map((n) => [n, ""])));
  // 还没填的变量在预览里保留 {{名字}}，提醒还要填
  const text = prepareForSend(render(snippet.body, Object.fromEntries(Object.entries(values).filter(([, v]) => v !== ""))));
  const ready = names.every((n) => values[n] !== "");
  return (
    <Dialog
      title={t("snippets.sendTitle", { title: snippet.title })}
      onDismiss={onClose}
      actions={<>
        <Button onClick={onClose}>{t("common.cancel")}</Button>
        <Button variant={execute ? "secondary" : "primary"} disabled={!ready || targets === 0} onClick={() => onSend(text, false)}>{t("snippets.paste")}</Button>
        <Button variant={execute ? "primary" : "secondary"} disabled={!ready || targets === 0} onClick={() => onSend(text, true)}>{t("snippets.run")}</Button>
      </>}
    >
      <div className="snip-form">
        {names.map((n, i) => (
          <TextField key={n} label={n} value={values[n]} autoFocus={i === 0} spellCheck={false} className="pl-input mono"
            onChange={(e) => setValues((v) => ({ ...v, [n]: e.target.value }))} />
        ))}
        <p className="snip__hint">{targets > 1 ? t("snippets.willSendBroadcast", { count: targets }) : t("snippets.willSend")}</p>
        <pre className="snip__body snip__body--preview mono">{text}</pre>
      </div>
    </Dialog>
  );
}
