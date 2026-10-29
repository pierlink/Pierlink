import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, ApiError } from "./api";
import icon from "../../app-icon.svg";

/** 猜一个设备名（只是默认值，用户可以改） */
function guessName(): string {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  if (/HarmonyOS/i.test(ua)) return "HarmonyOS";
  if (/Android/.test(ua)) return "Android";
  return "Phone";
}

/** 邀请码只留字母和数字并转成大写：粘贴进来的空格、横线去掉 */
export function cleanCode(s: string): string {
  return s.toUpperCase().replace(/[^A-Z0-9]/g, "").slice(0, 8);
}

/** 电脑上等批准的时间（和后端一致） */
const APPROVAL_S = 120;

/**
 * 配对：输入电脑上显示的 8 位邀请码（扫二维码进来时已经填好），等电脑上点“批准”。
 * 三个阶段：填写 → 等电脑批准（显示剩余秒数）→ 配对成功（停一下再进入主界面）。
 */
export function PairScreen({ onPaired }: { onPaired: () => void }) {
  const { t } = useTranslation();
  const fromLink = useRef(cleanCode(new URLSearchParams(location.search).get("code") ?? ""));
  const [code, setCode] = useState(fromLink.current);
  const [name, setName] = useState(guessName);
  const [phase, setPhase] = useState<"form" | "waiting" | "done">("form");
  const [left, setLeft] = useState(APPROVAL_S);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (phase !== "waiting") return;
    const start = Date.now();
    const timer = setInterval(() => setLeft(Math.max(0, APPROVAL_S - Math.round((Date.now() - start) / 1000))), 1000);
    return () => clearInterval(timer);
  }, [phase]);

  const pair = async () => {
    setPhase("waiting");
    setLeft(APPROVAL_S);
    setError(null);
    try {
      await api.pair(code, name.trim() || guessName());
      // 地址里的邀请码用过就去掉，免得分享截图时带出去
      history.replaceState(null, "", location.pathname);
      setPhase("done");
      setTimeout(onPaired, 800);
    } catch (e) {
      const c = e instanceof ApiError ? e.code : null;
      setPhase("form");
      setError(c === "mobile.invalid_invite" || c === "mobile.pair_denied" ? t("mobile.pair.denied")
        : c === "mobile.pair_timeout" ? t("mobile.pair.timeout")
        : c === "mobile.rate_limited" ? t("mobile.pair.tooMany")
        : c === "mobile.device_limit" ? t("mobile.pair.deviceLimit")
        : t("mobile.pair.failed"));
    }
  };

  const ready = code.length === 8;
  return (
    <div className="m-pair">
      <header className="m-pair__brand">
        <img src={icon} alt="" width={52} height={52} />
        <div>
          <h1>Pierlink</h1>
          <p className="m-muted">{t("mobile.pair.tagline")}</p>
        </div>
      </header>

      {phase === "done" && (
        <div className="m-card m-pair__state" role="status">
          <span className="m-pair__check" aria-hidden="true">✓</span>
          <b>{t("mobile.pair.success")}</b>
        </div>
      )}

      {phase === "waiting" && (
        <div className="m-card m-pair__state" role="status">
          <span className="m-spinner" aria-hidden="true" />
          <b>{t("mobile.pair.waiting")}</b>
          <p className="m-muted">{t("mobile.pair.waitingHint", { s: left })}</p>
        </div>
      )}

      {phase === "form" && (
        <form className="m-card m-pair__form" onSubmit={(e) => { e.preventDefault(); if (ready) void pair(); }}>
          <ol className="m-steps">
            <li>{t("mobile.pair.step1")}</li>
            <li>{t("mobile.pair.step2")}</li>
            <li>{t("mobile.pair.step3")}</li>
          </ol>
          <label className="m-field">
            <span>{t("mobile.pair.code")}</span>
            <input className="m-code-input" value={code} placeholder="••••••••" inputMode="text" autoCapitalize="characters" autoCorrect="off" spellCheck={false}
              autoComplete="one-time-code" onChange={(e) => setCode(cleanCode(e.target.value))} />
            {fromLink.current !== "" && code === fromLink.current && <span className="m-field__hint">{t("mobile.pair.codeFromLink")}</span>}
          </label>
          <label className="m-field">
            <span>{t("mobile.pair.name")}</span>
            <input value={name} maxLength={40} onChange={(e) => setName(e.target.value)} />
            <span className="m-field__hint">{t("mobile.pair.nameHint")}</span>
          </label>
          {error && <p className="m-alert" role="alert">{error}</p>}
          <button type="submit" className="m-btn m-btn--primary m-btn--block" disabled={!ready}>{t("mobile.pair.submit")}</button>
        </form>
      )}
    </div>
  );
}
