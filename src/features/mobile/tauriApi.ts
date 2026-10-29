import { listen } from "@tauri-apps/api/event";
import { commands } from "../../bindings/commands";
import type { MobileApi, MobileDevice, MobileStatus, PairRequest } from "./api";

function unwrap<T>(r: { status: "ok"; data: T } | { status: "error"; error: unknown }): T {
  if (r.status === "error") throw r.error;
  return r.data;
}

const status = (s: { port: number | null } & Omit<MobileStatus, "port">): MobileStatus => ({ ...s, port: s.port ?? 48600 });

export const tauriMobileApi: MobileApi = {
  status: async () => status(unwrap(await commands.mobileStatus())),
  configure: async (enabled, bind, port) => status(unwrap(await commands.mobileConfigure(enabled, bind, port))),
  invite: async () => { const r = unwrap(await commands.mobileInvite()); return { ...r, expiresAtMs: r.expiresAtMs ?? 0 }; },
  cancelInvite: async () => { unwrap(await commands.mobileCancelInvite()); },
  devices: async () => unwrap(await commands.mobileDevices()).map((d) => ({ ...d, createdMs: d.createdMs ?? 0, lastSeenMs: d.lastSeenMs ?? null, userAgent: d.userAgent ?? null, lastIp: d.lastIp ?? null }) as MobileDevice),
  setPermissions: async (id, p) => { unwrap(await commands.mobileSetPermissions(id, p)); },
  rename: async (id, name) => { unwrap(await commands.mobileRename(id, name)); },
  revoke: async (id) => { unwrap(await commands.mobileRevoke(id)); },
  revokeAll: async () => { unwrap(await commands.mobileRevoke(null)); },
  answerPair: async (id, allow) => unwrap(await commands.mobilePairAnswer(id, allow)),
  onPairRequest: (cb) => listen<PairRequest>("pierlink://mobile-pair", (e) => cb(e.payload)),
  onPairClosed: (cb) => listen<number>("pierlink://mobile-pair-closed", (e) => cb(e.payload)),
  onChanged: (cb) => listen("pierlink://mobile-changed", () => cb()),
};
