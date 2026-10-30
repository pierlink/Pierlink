import { useState } from "react";
import { useSettings } from "../features/settings/store";
import { NewSyncWizard } from "../features/sync/NewSyncWizard";
import type { PathIssue, SyncEstimateView } from "../features/sync/types";

const MB = 1024 * 1024;

const estimate: SyncEstimateView = {
  done: true, scannedRemote: 2148, scannedLocal: 297,
  upload: { files: 14, bytes: 1.4 * MB }, download: { files: 39, bytes: 6.2 * MB }, same: 254,
  conflicts: [
    { path: "configs/train.yaml", localTime: "10-20 18:02", remoteTime: "10-21 09:15", newer: "remote" },
    { path: "src/data/loader.py", localTime: "10-21 08:40", remoteTime: "10-19 22:10", newer: "local" },
  ],
  conflictCount: 23,
  remoteTotal: { files: 2148, bytes: 1331 * MB }, localTotal: { files: 297, bytes: 79 * MB },
  remoteIncluded: { files: 316, bytes: 84 * MB }, localIncluded: { files: 291, bytes: 78 * MB },
  included: [
    { path: "src/", files: 204, bytes: 1.9 * MB }, { path: "configs/", files: 38, bytes: 0.12 * MB },
    { path: "notebooks/", files: 17, bytes: 81 * MB }, { path: "train.py", files: 1, bytes: 14 * 1024 },
  ],
  excluded: [
    { path: ".venv/", reason: "dependency", bytes: 640 * MB }, { path: "checkpoints/", reason: "custom", bytes: 1100 * MB },
    { path: "__pycache__/", reason: "cache", bytes: null }, { path: ".env", reason: "sensitive", bytes: null }, { path: ".git/", reason: "git", bytes: 38 * MB },
  ],
};

/** 开发预览：新建同步任务向导。#sync-new-<场景>：paths（默认）/ issues（检查出问题）/ preview（直接到第三步） */
export function NewSyncDemo({ scenario }: { scenario: string }) {
  const [open, setOpen] = useState(true);
  const { settings } = useSettings();
  if (!open) return null;
  const issues: PathIssue[] = scenario === "issues"
    ? [{ kind: "dangerous", side: "remote", path: "/home/dev" }, { kind: "cloudFolder", provider: "OneDrive" }]
    : [];
  return (
    <NewSyncWizard
      servers={[{ id: "s1", name: "gpu-dev", deployed: true }, { id: "s2", name: "hk-web-01", deployed: false }]}
      defaultRules={settings.syncDefaults.ignoreRules}
      defaultConflictMode={settings.syncDefaults.conflictMode}
      checkPaths={async () => issues}
      estimate={() => new Promise((r) => setTimeout(() => r(estimate), 600))}
      onCreate={() => setOpen(false)}
      onClose={() => setOpen(false)}
      initial={scenario === "paths" ? undefined : { remotePath: "/home/dev/ml-train", localPath: "D:\\code\\ml-train", name: "ml-train" }}
      initialStep={scenario === "preview" ? 2 : scenario === "rules" ? 1 : 0}
    />
  );
}
