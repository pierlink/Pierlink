import { ExitDialog, type ExitSummary } from "../features/shell/ExitGuard";

const SCENARIOS: Record<string, { summary: ExitSummary | null; error: string | null }> = {
  checking: { summary: null, error: null },
  unsynced: {
    summary: {
      terminals: 3,
      syncUnknown: false,
      tasks: [
        { name: "web-app", needFiles: 128, needBytes: 52_428_800 },
        { name: "数据集", needFiles: 1, needBytes: 2_048 },
      ],
    },
    error: null,
  },
  terminals: { summary: { terminals: 2, syncUnknown: false, tasks: [] }, error: null },
  unknown: { summary: { terminals: 0, syncUnknown: true, tasks: [] }, error: null },
  failed: { summary: null, error: "同步服务没有响应" },
};

/** 开发预览：#exit-<场景>，场景为 checking / unsynced / terminals / unknown / failed */
export function ExitDemo({ scenario }: { scenario: string }) {
  const s = SCENARIOS[scenario] ?? SCENARIOS.unsynced;
  const noop = () => {};
  return <ExitDialog summary={s.summary} error={s.error} tray quitting={false} onCancel={noop} onTray={noop} onQuit={noop} />;
}
