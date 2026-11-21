import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Delete, X } from "lucide-react";
import { KEY_ROWS, MAX_PRESETS, MAX_STEPS, MODS, baseLabel, presetLabel, stepFor, stepLabel, type KeyPreset, type KeyStep, type Mod } from "./keys";

/**
 * 组合键面板：点 Ctrl、Alt、Shift 再点键，一步一步连成一组（如 Ctrl+A 再按 d），
 * 可以马上发出去，也可以存到键盘栏以后一点就发。
 */
export function KeySheet({ presets, disabled, onSend, onPresets, onClose }: {
  presets: KeyPreset[];
  /** 现在不能输入（在等密码、等危险确认） */
  disabled: boolean;
  onSend: (steps: KeyStep[]) => void;
  onPresets: (next: KeyPreset[]) => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const [mods, setMods] = useState<Mod[]>([]);
  const [steps, setSteps] = useState<KeyStep[]>([]);
  const label = presetLabel(steps);
  const saved = presets.some((p) => presetLabel(p.steps) === label);

  const toggle = (mod: Mod) => setMods((cur) => (cur.includes(mod) ? cur.filter((m) => m !== mod) : [...cur, mod]));
  const add = (step: KeyStep) => {
    setSteps((cur) => [...cur, step]);
    // 修饰键只管下一个键
    setMods([]);
  };

  return (
    // 点面板里的键时不让输入框失焦：键盘开着就一直开着，面板不会跟着键盘收起往下跳
    <div className="m-keysheet" role="dialog" aria-label={t("mobile.terminals.keys.title")} onPointerDown={(e) => e.preventDefault()}>
      <div className="m-keysheet__head">
        <span className="m-keysheet__steps" aria-live="polite">
          {steps.length === 0 ? <span className="m-muted">{t("mobile.terminals.keys.empty")}</span> : steps.map((s, i) => <kbd key={i} className="m-kbd">{stepLabel(s)}</kbd>)}
        </span>
        <button type="button" className="m-icon-btn" aria-label={t("mobile.terminals.keys.undo")} disabled={steps.length === 0} onClick={() => setSteps((cur) => cur.slice(0, -1))}><Delete size={20} /></button>
        <button type="button" className="m-icon-btn" aria-label={t("common.close")} onClick={onClose}><X size={20} /></button>
      </div>
      <div className="m-keysheet__grid">
        {MODS.map((mod) => (
          <button key={mod} type="button" aria-pressed={mods.includes(mod)} className={`m-key m-key--mod${mods.includes(mod) ? " m-key--on" : ""}`} onClick={() => toggle(mod)}>{mod}</button>
        ))}
      </div>
      {KEY_ROWS.map((row, i) => (
        <div key={i} className="m-keysheet__grid">
          {row.map((base) => {
            const step = stepFor(mods, base);
            return (
              <button key={base} type="button" className="m-key m-key--cell" disabled={!step || steps.length >= MAX_STEPS} onClick={() => step && add(step)}>{baseLabel(base)}</button>
            );
          })}
        </div>
      ))}
      <div className="m-actions">
        <button type="button" className="m-btn" disabled={steps.length === 0 || saved || presets.length >= MAX_PRESETS}
          onClick={() => { onPresets([...presets, { steps }]); setSteps([]); }}>
          {t("mobile.terminals.keys.save")}
        </button>
        <button type="button" className="m-btn m-btn--primary" disabled={steps.length === 0 || disabled} onClick={() => { onSend(steps); setSteps([]); }}>{t("mobile.terminals.send")}</button>
      </div>
      {presets.length > 0 && (
        <div className="m-keysheet__saved" aria-label={t("mobile.terminals.keys.saved")}>
          {presets.map((p, i) => (
            <span key={i} className="m-keysheet__preset">
              <kbd className="m-kbd">{presetLabel(p.steps)}</kbd>
              <button type="button" className="m-keysheet__remove" aria-label={t("mobile.terminals.keys.remove", { keys: presetLabel(p.steps) })} onClick={() => onPresets(presets.filter((_, n) => n !== i))}><X size={16} /></button>
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
