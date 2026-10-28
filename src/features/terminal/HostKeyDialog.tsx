import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, Copy, ShieldQuestion } from "lucide-react";
import { Button, Dialog, IconButton } from "../../components";
import type { HostKeyStep } from "./types";

type Confirm = Extract<HostKeyStep, { kind: "confirm" }>;

interface Props {
  step: Confirm;
  /** 正在把回答发给 ssh、等待下一步 */
  busy: boolean;
  onAccept: () => void;
  onCancel: () => void;
}

/** 服务器上核对指纹用的命令里，密钥文件名用小写的类型名 */
function hostKeyFile(keyType: string): string {
  const t = keyType.toLowerCase();
  const name = t.startsWith("ecdsa") ? "ecdsa" : t.includes("rsa") ? "rsa" : t.includes("ed25519") ? "ed25519" : t;
  return `/etc/ssh/ssh_host_${name}_key.pub`;
}

/**
 * 首次连接时核对服务器指纹。只有“取消”和“信任并连接”两个选择；
 * 关闭对话框等同取消（不会默认信任）。
 */
export function HostKeyDialog({ step, busy, onAccept, onCancel }: Props) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const copy = () => {
    void navigator.clipboard.writeText(step.fingerprint).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };
  const verifyCmd = `ssh-keygen -lf ${hostKeyFile(step.keyType)}`;

  return (
    <Dialog
      title={t("hostKey.title")}
      onDismiss={onCancel}
      actions={<>
        {/* 连接中也允许取消：服务器接受指纹后卡住时，不能让用户面对一个关不掉的对话框 */}
        <Button onClick={onCancel}>{t("common.cancel")}</Button>
        <Button variant="primary" onClick={onAccept} disabled={busy}>
          {busy ? t("hostKey.connecting") : t("hostKey.trust")}
        </Button>
      </>}
    >
      <div className="hk">
        <div className="hk__lead">
          <ShieldQuestion size={18} aria-hidden />
          <p>
            {t("hostKey.lead", { host: step.host })}
            {step.level > 1 && <> {t("hostKey.jumpLevel", { level: step.level })}</>}
          </p>
        </div>
        <dl className="hk__facts">
          <dt>{t("hostKey.host")}</dt>
          <dd className="mono">{step.host}</dd>
          <dt>{t("hostKey.keyType")}</dt>
          <dd className="mono">{step.keyType}</dd>
          <dt>{t("hostKey.fingerprint")}</dt>
          <dd className="hk__fp">
            <code className="mono">{step.fingerprint}</code>
            <IconButton label={copied ? t("hostKey.copied") : t("hostKey.copy")} size="sm" onClick={copy}>
              {copied ? <Check size={14} /> : <Copy size={14} />}
            </IconButton>
          </dd>
        </dl>
        <p className="hk__hint">
          {t("hostKey.verifyHint")}
          <code className="mono hk__cmd">{verifyCmd}</code>
        </p>
      </div>
    </Dialog>
  );
}
