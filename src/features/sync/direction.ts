/** 同步方向：由本机一侧和各服务器一侧的方式组合而成 */
export type FolderMode = "sendReceive" | "receiveOnly";
export type Direction = "both" | "toServer" | "toLocal";

/**
 * 两端方式 → 方向。服务器各端方式不一致（多服务器任务里单独改过）时返回 mixed。
 * - 双向：两边都收发；
 * - 本机 → 服务器：服务器只接收；
 * - 服务器 → 本机：本机只接收。
 */
export function directionOf(localMode: FolderMode, serverModes: FolderMode[]): Direction | "mixed" {
  if (localMode === "receiveOnly") return serverModes.every((m) => m === "sendReceive") ? "toLocal" : "mixed";
  if (serverModes.length && serverModes.every((m) => m === "receiveOnly")) return "toServer";
  if (serverModes.every((m) => m === "sendReceive")) return "both";
  return "mixed";
}

/** 方向 → 本机方式和服务器方式 */
export function modesOf(d: Direction): { localMode: FolderMode; serverMode: FolderMode } {
  if (d === "toServer") return { localMode: "sendReceive", serverMode: "receiveOnly" };
  if (d === "toLocal") return { localMode: "receiveOnly", serverMode: "sendReceive" };
  return { localMode: "sendReceive", serverMode: "sendReceive" };
}
