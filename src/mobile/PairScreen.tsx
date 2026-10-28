import { useState } from "react";
import { useTranslation } from "react-i18next";
import { api, ApiError } from "./api";

/** 猜一个设备名（只是默认值，用户可以改） */
function guessName(): string {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  if (/Android/.test(ua)) return "Android";
  return "Phone";
}

/**
 * 配对：输入电脑上显示的 8 位邀请码（扫二维码进来时已经带上），等电脑上点“批准”。
 */
export function PairScreen({ onPaired }: { onPaired: () => void }) {
  const { t } = useTranslation();
  const [code, setCode] = useState(() => (new URLSearchParams(location.search).get("code") ?? "").toUpperCase());
  const [name, setName] = useState(guessName);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const pair = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.pair(code.trim().toUpperCase(), name.trim() || guessName());
      // 地址里的邀请码用过就去掉，免得分享截图时带出去
      history.replaceState(null, "", location.pathname);
      onPaired();
    } catch (e) {
      const code = e instanceof ApiError ? e.code : null;
      setError(code === "mobile.invalid_invite" || code === "mobile.pair_denied" ? t("mobile.pair.denied")
        : code === "mobile.pair_timeout" ? t("mobile.pair.timeout")
        : code === "mobile.rate_limited" ? t("mobile.pair.tooMany")
        : code === "mobile.device_limit" ? t("mobile.pair.deviceLimit")
        : t("mobile.pair.failed"));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="m-pair">
      <h1>Pierlink</h1>
      <p className="m-muted">{t("mobile.pair.intro")}</p>
      <label className="m-field">
        <span>{t("mobile.pair.code")}</span>
        <input className="m-code-input" value={code} inputMode="text" autoCapitalize="characters" autoComplete="one-time-code" maxLength={8} onChange={(e) => setCode(e.target.value.toUpperCase())} />
      </label>
      <label className="m-field">
        <span>{t("mobile.pair.name")}</span>
        <input value={name} maxLength={40} onChange={(e) => setName(e.target.value)} />
      </label>
      <button type="button" className="m-btn m-btn--primary" disabled={busy || code.trim().length !== 8} onClick={() => void pair()}>
        {busy ? t("mobile.pair.waiting") : t("mobile.pair.submit")}
      </button>
      <p className="m-status" role="status">{error ?? (busy ? t("mobile.pair.waitingHint") : "")}</p>
    </div>
  );
}
