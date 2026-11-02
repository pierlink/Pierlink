import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { RotateCcw, X } from "lucide-react";
import { Button, IconButton } from "../../components";
import { useSettings } from "../settings/store";
import { SHORTCUT_ACTIONS, SHORTCUT_GROUPS, checkBinding, restoreBlockedBy, shortcutAction, withBinding, type BindCheck, type ShortcutId } from "./actions";
import { chordFromEvent, formatCombo, formatHeld, parseCombo, terminalUses } from "./keys";
import { pauseShortcuts, useShortcutBindings } from "./runtime";
import "./shortcuts.css";

/**
 * 设置 → 快捷键：列出所有动作和现在的快捷键。点按键进入录制，按下组合键就改；
 * 和别的动作重复、是保留键、只按了修饰键或没带 Ctrl/Alt 时不保存，在这一行写明原因。
 * 每一行的提示位置一直占着，提示出现、消失时布局不动。
 */
export function ShortcutsSection() {
  const { t } = useTranslation();
  const { settings, update } = useSettings();
  const bindings = useShortcutBindings();
  const [recording, setRecording] = useState<ShortcutId | null>(null);
  /** 正在录制的那个按键按钮 */
  const recButton = useRef<HTMLButtonElement | null>(null);
  /** 录制时已经按下的修饰键，如“Ctrl+Shift+” */
  const [held, setHeld] = useState("");
  /** 某一行没保存的原因（冲突等）：开始下一次录制或改了任何一行时清掉 */
  const [problem, setProblem] = useState<{ id: ShortcutId; text: string } | null>(null);

  const name = (id: ShortcutId) => t(`shortcuts.action.${id}`);
  const save = (id: ShortcutId, combo: string | null) => {
    setProblem(null);
    update((s) => ({ ...s, shortcuts: withBinding(s.shortcuts, id, combo) }));
  };
  const stop = () => {
    setRecording(null);
    setHeld("");
  };
  const reason = (r: Extract<BindCheck, { ok: false }>): string => {
    switch (r.reason) {
      case "taken": return t("shortcuts.taken", { key: formatCombo(r.combo), name: name(r.by) });
      case "reserved": return t("shortcuts.reservedFor", { key: formatCombo(r.combo), use: t(`shortcuts.use.${r.use}`) });
      case "needModifier": return t("shortcuts.needModifier");
      case "winKey": return t("shortcuts.winKey");
      case "unsupported": return t("shortcuts.unsupported");
      case "modifierOnly": return "";
    }
  };

  // 录制：整个快捷键监听暂停，所有按键都拦下（Tab 不移走焦点，Ctrl+W 之类也不触发别的）；Esc 取消
  useEffect(() => {
    if (!recording) return;
    const resume = pauseShortcuts();
    const onKey = (e: KeyboardEvent) => {
      // 焦点已经不在录制的按钮上（页面被切走等，按钮没收到 blur）：结束录制，这个键照常交给别处
      if (document.activeElement !== recButton.current) { stop(); return; }
      e.preventDefault();
      e.stopPropagation();
      e.stopImmediatePropagation();
      const chord = chordFromEvent(e);
      if (e.type === "keyup") { setHeld(chord ? formatHeld(chord) : ""); return; }
      if (e.repeat || e.isComposing) return;
      if (e.code === "Escape" && !e.ctrlKey && !e.altKey && !e.shiftKey && !e.metaKey) { stop(); return; }
      if (!chord) return;
      // 只按了修饰键：等着按下一个键
      if (!chord.code) { setHeld(formatHeld(chord)); return; }
      const result = checkBinding(recording, chord, bindings);
      if (result.ok) {
        if (result.combo !== bindings[recording]) save(recording, result.combo);
        stop();
        return;
      }
      setHeld("");
      setProblem({ id: recording, text: reason(result) });
    };
    window.addEventListener("keydown", onKey, true);
    window.addEventListener("keyup", onKey, true);
    return () => {
      resume();
      window.removeEventListener("keydown", onKey, true);
      window.removeEventListener("keyup", onKey, true);
    };
    // reason、save 只用到 t 和 update，都不变
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recording, bindings]);

  const restore = (id: ShortcutId) => {
    const by = restoreBlockedBy(id, bindings);
    if (by) {
      setProblem({ id, text: t("shortcuts.restoreTaken", { key: formatCombo(shortcutAction(id).combo), name: name(by) }) });
      return;
    }
    save(id, shortcutAction(id).combo);
  };

  /** 这一行提示位置上显示的：没保存的原因 > 终端用到这个键 > 改过时的默认键 */
  const note = (id: ShortcutId): { tone: "danger" | "warn" | "muted"; text: string } => {
    if (problem?.id === id) return { tone: "danger", text: problem.text };
    const combo = bindings[id];
    const chord = combo ? parseCombo(combo) : null;
    if (chord && terminalUses(chord)) return { tone: "warn", text: t("shortcuts.terminalUses") };
    const def = shortcutAction(id).combo;
    if (combo !== def) return { tone: "muted", text: def ? t("shortcuts.defaultKey", { key: formatCombo(def) }) : t("shortcuts.defaultNone") };
    return { tone: "muted", text: "" };
  };

  return (
    <>
      <div className="sc-head">
        <p className="set-lead">{t("shortcuts.lead")}</p>
        <Button size="sm" variant="ghost" disabled={!Object.keys(settings.shortcuts).length}
          onClick={() => { stop(); setProblem(null); update((s) => ({ ...s, shortcuts: {} })); }}>
          {t("shortcuts.resetAll")}
        </Button>
      </div>
      {SHORTCUT_GROUPS.map((group) => (
        <div key={group} className="sc-group">
          <h3 className="sc-group__title">{t(`shortcuts.group.${group}`)}</h3>
          <div className="set-card">
            {SHORTCUT_ACTIONS.filter((a) => a.group === group).map(({ id, combo: def }) => {
              const combo = bindings[id];
              const text = formatCombo(combo);
              const rec = recording === id;
              const n = note(id);
              return (
                <div key={id} className="sc-row" data-shortcut={id}>
                  <span className="sc-row__name">{name(id)}</span>
                  <span className={`sc-row__note sc-row__note--${n.tone}`} role={n.tone === "danger" ? "alert" : undefined} title={n.text || undefined}>{n.text}</span>
                  <button type="button" className={`sc-key mono${text || rec ? "" : " sc-key--empty"}`} aria-pressed={rec}
                    aria-label={`${name(id)}: ${text || t("shortcuts.none")}`}
                    onClick={(e) => {
                      if (rec) { stop(); return; }
                      recButton.current = e.currentTarget;
                      setProblem(null);
                      setHeld("");
                      setRecording(id);
                    }}
                    onBlur={() => { if (rec) stop(); }}>
                    {rec ? (held ? `${held}…` : t("shortcuts.press")) : text || t("shortcuts.none")}
                  </button>
                  <IconButton size="sm" label={t("shortcuts.reset")} disabled={combo === def} onClick={() => restore(id)}><RotateCcw size={13} /></IconButton>
                  <IconButton size="sm" label={t("shortcuts.clear")} disabled={!combo} onClick={() => save(id, null)}><X size={13} /></IconButton>
                </div>
              );
            })}
          </div>
        </div>
      ))}
      <p className="set-lead sc-reserved">{t("shortcuts.reserved")}</p>
    </>
  );
}
