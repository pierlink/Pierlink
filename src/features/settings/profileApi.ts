import { inTauri } from "../shell/TitleBar";

/** 导入结果（和后端一致） */
export interface ImportResult {
  serversAdded: number;
  forwardsAdded: number;
  snippetsAdded: number;
  notes: { name: string; reason: string; path: string | null }[];
  mcpEntriesAdded: number;
  appSettings: string | null;
}

const SAMPLE: ImportResult = {
  serversAdded: 4, forwardsAdded: 3, snippetsAdded: 12, mcpEntriesAdded: 1,
  notes: [{ name: "gpu-dev", reason: "duplicate", path: null }, { name: "lab-nas", reason: "missing_key", path: "C:\\Users\\old\\.ssh\\id_lab" }],
  appSettings: "{}",
};

async function cmds() {
  return (await import("../../bindings/commands")).commands;
}

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

type RawResult = Partial<ImportResult> & { notes: ImportResult["notes"]; appSettings: string | null };
function normalize(r: RawResult | null): ImportResult | null {
  return r ? { ...r, serversAdded: r.serversAdded ?? 0, forwardsAdded: r.forwardsAdded ?? 0, snippetsAdded: r.snippetsAdded ?? 0, mcpEntriesAdded: r.mcpEntriesAdded ?? 0 } : null;
}

/** 完整备份导出：返回文件路径，取消时 null */
export async function exportProfile(appSettings: string): Promise<string | null> {
  if (!inTauri) return "D:\\Users\\you\\Documents\\pierlink-20261026.json";
  return unwrap(await (await cmds()).profileExport(appSettings));
}

/** 完整备份导入：返回结果，取消时 null */
export async function importProfile(): Promise<ImportResult | null> {
  if (!inTauri) return SAMPLE;
  return normalize(unwrap(await (await cmds()).profileImport()) as RawResult | null);
}

/** 只导出选中的服务器（给别人用）：返回文件路径，取消时 null */
export async function exportServers(ids: string[]): Promise<string | null> {
  if (!inTauri) return "D:\\Users\\you\\Documents\\pierlink-servers-20261026.json";
  return unwrap(await (await cmds()).serversExport(ids));
}

/** 从文件只导入服务器：返回结果，取消时 null */
export async function importServers(): Promise<ImportResult | null> {
  if (!inTauri) return { ...SAMPLE, forwardsAdded: 0, snippetsAdded: 0, mcpEntriesAdded: 0, appSettings: null };
  return normalize(unwrap(await (await cmds()).serversImport()) as RawResult | null);
}
