import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Monitor, Moon, Sun } from "lucide-react";
import { Banner, Button, Select, Switch, type SelectOption } from "../../components";
import { listLocalShells, localShellName, type LocalShellView } from "../terminal/backend";
import { DEFAULT_IGNORE_RULES, type AppSettings } from "./model";
import { useSettings } from "./store";
import { inTauri } from "../shell/TitleBar";
import { trayAvailable } from "../shell/ExitGuard";
import { getUpdatesApi } from "../updates/api";
import { announceUpdate } from "../updates/UpdateNotice";
import { formatError } from "../../errors";
import { BackupSection } from "./BackupSection";
import { MobileSection } from "../mobile/MobileSection";
import { clearHistory } from "../terminal/history";
import "./settings.css";

type Section = "appearance" | "terminal" | "desktop" | "sync" | "phone" | "backup" | "about";
const SECTIONS: Section[] = ["appearance", "terminal", "desktop", "sync", "phone", "backup", "about"];

/** 一行设置：左边说明，右边控件 */
function Row({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="set-row">
      <span className="set-row__text">
        <span>{label}</span>
        {hint && <span className="set-row__hint">{hint}</span>}
      </span>
      <span className="set-row__control">{children}</span>
    </div>
  );
}

/** 行内下拉框：说明文字已在行的左边，这里只给读屏用的名称 */
function RowSelect(props: { label: string; value: string; onChange: (v: string) => void; options: SelectOption[] }) {
  return <Select {...props} />;
}

/**
 * 数字输入：失去焦点或回车时才提交，避免输入过程中被范围限制打断。
 * 用普通文本框（只接受数字）：没有上下箭头，滚轮也不会改值。
 */
function NumberInput({ value, min, max, onCommit, label, suffix }: { value: number; min: number; max: number; onCommit: (v: number) => void; label: string; suffix?: string }) {
  const [text, setText] = useState(String(value));
  useEffect(() => setText(String(value)), [value]);
  const commit = () => {
    const v = Number(text);
    if (Number.isFinite(v) && text !== "") {
      const clamped = Math.min(max, Math.max(min, Math.round(v)));
      // 存储值本来就等于夹后的值时不会触发更新，这里直接把框里的文字改成实际生效的值
      setText(String(clamped));
      onCommit(clamped);
    } else setText(String(value));
  };
  return (
    <span className="set-num">
      <input className="pl-input mono" type="text" inputMode="numeric" autoComplete="off" aria-label={label} value={text}
        onChange={(e) => setText(e.target.value.replace(/[^0-9]/g, "").slice(0, 9))} onBlur={commit} onKeyDown={(e) => { if (e.key === "Enter") commit(); }} />
      {suffix && <span className="set-num__suffix">{suffix}</span>}
    </span>
  );
}

function ThemeCard({ value, current, onPick }: { value: AppSettings["theme"]; current: AppSettings["theme"]; onPick: () => void }) {
  const { t } = useTranslation();
  const icon = value === "dark" ? <Moon size={16} /> : value === "light" ? <Sun size={16} /> : <Monitor size={16} />;
  const label = t(`settings.theme${value[0].toUpperCase()}${value.slice(1)}`);
  return (
    <button type="button" className={`theme-card theme-card--${value}`} aria-pressed={value === current} onClick={onPick}>
      <span className="theme-card__preview" aria-hidden><span /><span /></span>
      <span className="theme-card__label">{icon}{label}</span>
    </button>
  );
}

/** 设置页：左侧分类导航，右侧所有设置连续排列，点分类滚动到对应位置 */
export function SettingsPage({ version }: { version: string | null }) {
  const { t } = useTranslation();
  const { settings, update } = useSettings();
  const [shells, setShells] = useState<LocalShellView[]>([]);
  const [rulesText, setRulesText] = useState(settings.syncDefaults.ignoreRules.join("\n"));
  const [active, setActive] = useState<Section>("appearance");
  const refs = useRef<Record<Section, HTMLElement | null>>({ appearance: null, terminal: null, desktop: null, sync: null, phone: null, backup: null, about: null });
  const term = settings.terminal;
  const sync = settings.syncDefaults;

  // 浏览器预览没有托盘，按可用显示以便查看界面
  const [tray, setTray] = useState(!inTauri);
  useEffect(() => { void listLocalShells().then(setShells); }, []);
  useEffect(() => { if (inTauri) void trayAvailable().then(setTray); }, []);

  const mainRef = useRef<HTMLDivElement>(null);
  // 点左边分类平滑滚动期间，不让滚动跟随改掉刚点的分类
  const clickedAt = useRef(0);
  const go = (s: Section) => {
    setActive(s);
    clickedAt.current = Date.now();
    refs.current[s]?.scrollIntoView({ behavior: "smooth", block: "start" });
  };
  // 滚动时左边分类跟随：取顶部已经滚过的最后一个分区；滚到底时选最后一个
  useEffect(() => {
    const main = mainRef.current;
    if (!main) return;
    const onScroll = () => {
      if (Date.now() - clickedAt.current < 800) return;
      const top = main.getBoundingClientRect().top + 48;
      let current: Section = SECTIONS[0];
      for (const sec of SECTIONS) {
        const el = refs.current[sec];
        if (el && el.getBoundingClientRect().top <= top) current = sec;
      }
      if (main.scrollTop + main.clientHeight >= main.scrollHeight - 4) current = SECTIONS[SECTIONS.length - 1];
      setActive(current);
    };
    main.addEventListener("scroll", onScroll, { passive: true });
    return () => main.removeEventListener("scroll", onScroll);
  }, []);
  const [checking, setChecking] = useState(false);
  const [historyCleared, setHistoryCleared] = useState(false);
  const [historyError, setHistoryError] = useState<string | null>(null);
  const [updateResult, setUpdateResult] = useState<{ tone: "ok" | "accent" | "danger"; text: string } | null>(null);
  const checkUpdate = () => {
    setChecking(true);
    setUpdateResult(null);
    getUpdatesApi()
      .then((api) => api.check())
      .then((u) => {
        if (u) { announceUpdate(u); setUpdateResult({ tone: "accent", text: t("updates.available", { version: u.version }) }); }
        else setUpdateResult({ tone: "ok", text: t("updates.upToDate") });
      }, (e: unknown) => setUpdateResult({ tone: "danger", text: t("updates.checkFailed", { text: formatError(e) }) }))
      .finally(() => setChecking(false));
  };
  const setTerm = (patch: Partial<AppSettings["terminal"]>) => update((s) => ({ ...s, terminal: { ...s.terminal, ...patch } }));
  const setSync = (patch: Partial<AppSettings["syncDefaults"]>) => update((s) => ({ ...s, syncDefaults: { ...s.syncDefaults, ...patch } }));
  const rules = rulesText.split(/\r?\n/).map((x) => x.trim()).filter(Boolean);
  const rulesDirty = rules.join("\n") !== sync.ignoreRules.join("\n");

  return (
    <div className="settings">
      <nav className="pl-panel settings__nav" aria-label={t("settings.sections")}>
        {SECTIONS.map((s) => (
          <button key={s} type="button" className="settings__nav-item" aria-current={active === s ? "true" : undefined} onClick={() => go(s)}>
            {t(`settings.section.${s}`)}
          </button>
        ))}
      </nav>

      <div ref={mainRef} className="pl-panel settings__main">
        <section ref={(el) => { refs.current.appearance = el; }} className="set-section">
          <h2>{t("settings.section.appearance")}</h2>
          <div className="theme-cards" role="group" aria-label={t("settings.theme")}>
            {(["dark", "light", "system"] as const).map((v) => (
              <ThemeCard key={v} value={v} current={settings.theme} onPick={() => update((s) => ({ ...s, theme: v }))} />
            ))}
          </div>
          <div className="set-card">
            <Row label={t("settings.language")}>
              <RowSelect
                label={t("settings.language")}
                value={settings.language}
                onChange={(v) => update((s) => ({ ...s, language: v as AppSettings["language"] }))}
                options={[
                  { value: "system", label: t("settings.languageSystem") },
                  { value: "zh-CN", label: "简体中文" },
                  { value: "en", label: "English" },
                ]}
              />
            </Row>
            <Row label={t("settings.closeToTray")} hint={tray ? t("settings.closeToTrayHint") : t("settings.closeToTrayUnavailable")}>
              <Switch label={t("settings.closeToTray")} checked={settings.closeToTray && tray} disabled={!tray} onChange={(v) => update((s) => ({ ...s, closeToTray: v }))} />
            </Row>
          </div>
        </section>

        <section ref={(el) => { refs.current.terminal = el; }} className="set-section">
          <h2>{t("settings.section.terminal")}</h2>
          <div className="set-card">
            <Row label={t("settings.term.defaultShell")} hint={t("settings.term.defaultShellHint")}>
              <RowSelect
                label={t("settings.term.defaultShell")}
                value={term.defaultShell ?? ""}
                onChange={(v) => setTerm({ defaultShell: v || null })}
                options={[{ value: "", label: t("settings.term.auto") }, ...shells.map((s) => ({ value: s.id, label: localShellName(s) }))]}
              />
            </Row>
            <Row label={t("settings.term.fontSize")}>
              <NumberInput label={t("settings.term.fontSize")} value={term.fontSize} min={10} max={24} onCommit={(v) => setTerm({ fontSize: v })} suffix="px" />
            </Row>
            <Row label={t("settings.term.cursor")}>
              <RowSelect
                label={t("settings.term.cursor")}
                value={term.cursorStyle}
                onChange={(v) => setTerm({ cursorStyle: v as AppSettings["terminal"]["cursorStyle"] })}
                options={(["bar", "block", "underline"] as const).map((v) => ({ value: v, label: t(`settings.term.cursorStyle.${v}`) }))}
              />
            </Row>
            {term.cursorStyle === "bar" && (
              <Row label={t("settings.term.cursorWidth")}>
                <RowSelect
                  label={t("settings.term.cursorWidth")}
                  value={String(term.cursorWidth)}
                  onChange={(v) => setTerm({ cursorWidth: Number(v) as AppSettings["terminal"]["cursorWidth"] })}
                  options={([1, 2, 3] as const).map((v) => ({ value: String(v), label: t(`settings.term.cursorWidthOption.${v}`) }))}
                />
              </Row>
            )}
            <Row label={t("settings.term.cursorBlink")}>
              <Switch label={t("settings.term.cursorBlink")} checked={term.cursorBlink} onChange={(v) => setTerm({ cursorBlink: v })} />
            </Row>
            <Row label={t("settings.term.scrollback")} hint={t("settings.term.scrollbackHint")}>
              <NumberInput label={t("settings.term.scrollback")} value={term.scrollback} min={1000} max={100000} onCommit={(v) => setTerm({ scrollback: v })} />
            </Row>
            <Row label={t("settings.term.rightClickCopyPaste")} hint={t("settings.term.rightClickCopyPasteHint")}>
              <Switch label={t("settings.term.rightClickCopyPaste")} checked={term.rightClickCopyPaste} onChange={(v) => setTerm({ rightClickCopyPaste: v })} />
            </Row>
            <Row label={t("settings.term.pasteConfirm")} hint={t("settings.term.pasteConfirmHint")}>
              <Switch label={t("settings.term.pasteConfirm")} checked={term.confirmMultilinePaste} onChange={(v) => setTerm({ confirmMultilinePaste: v })} />
            </Row>
            <Row label={t("settings.term.broadcastDanger")} hint={t("settings.term.broadcastDangerHint")}>
              <Switch label={t("settings.term.broadcastDanger")} checked={term.broadcastConfirmDanger} onChange={(v) => setTerm({ broadcastConfirmDanger: v })} />
            </Row>
            <Row label={t("settings.term.closeConfirm")} hint={t("settings.term.closeConfirmHint")}>
              <Switch label={t("settings.term.closeConfirm")} checked={term.confirmClose} onChange={(v) => setTerm({ confirmClose: v })} />
            </Row>
            <Row label={t("settings.term.prediction")} hint={t("settings.term.predictionHint")}>
              <Switch label={t("settings.term.prediction")} checked={term.commandPrediction} onChange={(v) => setTerm({ commandPrediction: v })} />
            </Row>
            <Row label={t("settings.term.clearHistory")} hint={historyCleared ? t("settings.term.historyCleared") : undefined}>
              <Button size="sm" onClick={() => { void clearHistory().then(() => setHistoryCleared(true), (e: unknown) => setHistoryError(formatError(e))); }}>{t("settings.term.clearHistoryButton")}</Button>
            </Row>
            {historyError && <Banner tone="danger" title={historyError} />}
            <Row label={t("settings.term.shortcuts")} hint={t("settings.term.shortcutsHint")}>
              <span className="set-kbd mono">Ctrl+C / Ctrl+V / Ctrl+Shift+B</span>
            </Row>
          </div>
        </section>

        <section ref={(el) => { refs.current.desktop = el; }} className="set-section">
          <h2>{t("settings.section.desktop")}</h2>
          <div className="set-card">
            <Row label={t("settings.desktop.openInWindow")} hint={t("settings.desktop.openInWindowHint")}>
              <Switch label={t("settings.desktop.openInWindow")} checked={settings.desktop.openInWindow} onChange={(v) => update((s) => ({ ...s, desktop: { ...s.desktop, openInWindow: v } }))} />
            </Row>
          </div>
        </section>

        <section ref={(el) => { refs.current.sync = el; }} className="set-section">
          <h2>{t("settings.section.sync")}</h2>
          <p className="set-lead">{t("settings.sync.lead")}</p>
          <div className="set-card">
            <Row label={t("sync.watchDelay")} hint={t("settings.sync.watchDelayHint")}>
              <NumberInput label={t("sync.watchDelay")} value={sync.watchDelayS} min={1} max={60} onCommit={(v) => setSync({ watchDelayS: v })} suffix={t("settings.sync.seconds")} />
            </Row>
            <Row label={t("sync.conflictMode")}>
              <RowSelect
                label={t("sync.conflictMode")}
                value={sync.conflictMode}
                onChange={(v) => setSync({ conflictMode: v as "ask" | "newest" })}
                options={[{ value: "ask", label: t("sync.conflictModes.ask") }, { value: "newest", label: t("sync.conflictModes.newest") }]}
              />
            </Row>
            <Row label={t("sync.versions")} hint={t("settings.sync.versionsHint")}>
              <Switch label={t("sync.versions")} checked={sync.keepVersions} onChange={(v) => setSync({ keepVersions: v })} />
            </Row>
          </div>
          {!sync.keepVersions && <Banner tone="warn" title={t("settings.sync.versionsOffTitle")}>{t("settings.sync.versionsOffBody")}</Banner>}
          {sync.conflictMode === "newest" && <Banner tone="info" title={t("settings.sync.newestTitle")}>{t("settings.sync.newestBody")}</Banner>}
          <div className="set-card set-rules">
            <label className="wiz-field">
              <span className="wiz-field__label">{t("settings.sync.rules", { count: rules.length })}</span>
              <textarea className="pl-input mono wiz-rules" rows={10} spellCheck={false} value={rulesText} onChange={(e) => setRulesText(e.target.value)} />
            </label>
            <div className="set-rules__actions">
              <span className="sync-muted">{t("newSync.rulesNoteSyntax")}</span>
              <span style={{ flex: 1 }} />
              <Button size="sm" variant="ghost" onClick={() => setRulesText(DEFAULT_IGNORE_RULES.join("\n"))}>{t("newSync.rulesReset")}</Button>
              <Button size="sm" variant="primary" disabled={!rulesDirty} onClick={() => setSync({ ignoreRules: rules })}>{t("common.save")}</Button>
            </div>
          </div>
        </section>

        <section ref={(el) => { refs.current.phone = el; }} className="set-section">
          <h2>{t("settings.section.phone")}</h2>
          <MobileSection />
        </section>

        <section ref={(el) => { refs.current.backup = el; }} className="set-section">
          <h2>{t("settings.section.backup")}</h2>
          <BackupSection />
        </section>

        <section ref={(el) => { refs.current.about = el; }} className="set-section">
          <h2>{t("settings.section.about")}</h2>
          <div className="set-card">
            <Row label="Pierlink"><span className="mono">{version ?? t("settings.about.devBuild")}</span></Row>
            {inTauri && (
              <Row label={t("updates.title")}>
                {/* 结果放在按钮左边固定宽度的位置：出现、消失都不挤动布局 */}
                <span className={`upd-status${updateResult ? ` upd-status--${updateResult.tone}` : ""}`} role="status" title={updateResult?.text}>
                  {checking ? t("updates.checking") : updateResult?.text ?? ""}
                </span>
                <Button size="sm" disabled={checking} onClick={checkUpdate}>{t("updates.check")}</Button>
              </Row>
            )}
            <Row label={t("updates.autoCheck")} hint={t("updates.autoCheckHint")}>
              <Switch label={t("updates.autoCheck")} checked={settings.updates.autoCheck} onChange={(v) => update((s) => ({ ...s, updates: { ...s.updates, autoCheck: v } }))} />
            </Row>
            <Row label={t("settings.about.license")}><span className="mono">FSL-1.1-ALv2</span></Row>
            <Row label={t("settings.about.engine")}>{/* 与 scripts/fetch-bundled.mjs 里固定的版本一致 */}<span className="mono">Syncthing 2.1.7 · Win32-OpenSSH 10.0p2</span></Row>
            <Row label={t("settings.about.licenses")} hint={t("settings.about.licensesHint")}>
              <span className="mono">THIRD-PARTY-NOTICES.txt</span>
            </Row>
          </div>
        </section>
      </div>
    </div>
  );
}
