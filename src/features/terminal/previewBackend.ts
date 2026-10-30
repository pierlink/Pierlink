import type { OpenSpec, TerminalBackend, TerminalEvent } from "./types";

const enc = new TextEncoder();

/**
 * 浏览器里调试界面用的预览后端：不连接任何东西，只回显输入。
 * 运行在 Tauri 里时不会用到它。
 */
export function createPreviewBackend(banner: (spec: OpenSpec) => string, promptFor?: (spec: OpenSpec) => string): TerminalBackend {
  const sessions = new Map<string, { emit: (e: TerminalEvent) => void; line: string; busy: boolean; prompt: string }>();
  let seq = 0;
  const defaultPrompt = "\x1b[32mpreview\x1b[0m:\x1b[34m~\x1b[0m$ ";
  const knownHosts = new Set<string>();
  const probes = new Map<string, string>();
  return {
    preview: true,
    async open(spec, _cols, _rows, onEvent) {
      const id = `preview-${++seq}`;
      const prompt = promptFor?.(spec) ?? defaultPrompt;
      sessions.set(id, { emit: onEvent, line: "", busy: false, prompt });
      queueMicrotask(() => onEvent({ kind: "output", data: enc.encode(`${banner(spec)}\r\n${prompt}`) }));
      return id;
    },
    async write(id, data) {
      const s = sessions.get(id);
      if (!s) return;
      const prompt = s.prompt;
      let out = "";
      for (const ch of data) {
        if (ch === "\r") {
          if (s.line.trim() === "exit") {
            s.emit({ kind: "output", data: enc.encode("\r\n") });
            s.emit({ kind: "exited", code: 0, closedByUser: false });
            sessions.delete(id);
            return;
          }
          // 预览里用 sleep 模拟“有程序在运行”：不出提示符，直到 Ctrl+C
          if (/^sleep\b/.test(s.line.trim())) { out += "\r\n"; s.busy = true; s.line = ""; continue; }
          out += `\r\n${s.line ? `${s.line}\r\n` : ""}${prompt}`;
          s.line = "";
        } else if (ch === "\x7f") {
          if (s.line) { s.line = s.line.slice(0, -1); out += "\b \b"; }
        } else if (ch === "\x03") {
          out += `^C\r\n${prompt}`;
          s.line = "";
          s.busy = false;
        } else if (ch >= " ") {
          s.line += ch;
          out += ch;
        }
      }
      if (out) s.emit({ kind: "output", data: enc.encode(out) });
    },
    async checkHostKey(target) {
      const host = target.kind === "server" ? target.serverId : target.host;
      const port = target.kind === "server" ? 22 : target.port;
      const key = `${host}:${port}`;
      if (knownHosts.has(key)) return { kind: "trusted" };
      const probeId = `preview-hk-${++seq}`;
      probes.set(probeId, key);
      return { kind: "confirm", probeId, host: port === 22 ? host : `[${host}]:${port}`, level: 1, keyType: "ED25519", fingerprint: "SHA256:Xq3nB8vYk2Rz7tW1pLm9cH4dF6sJ0aEuGiOyTwQrVbM" };
    },
    async answerHostKey(probeId, accept) {
      const key = probes.get(probeId);
      probes.delete(probeId);
      if (!key) throw { code: "ssh.host_key_expired", params: {} };
      if (!accept) throw { code: "ssh.host_key_rejected", params: {} };
      knownHosts.add(key);
      return { kind: "trusted" };
    },
    async cancelHostKey(probeId) { probes.delete(probeId); },
    async busy(id) { return sessions.get(id)?.busy ?? null; },
    async resize() {},
    async ack() {},
    async close(id) {
      const s = sessions.get(id);
      sessions.delete(id);
      s?.emit({ kind: "exited", code: null, closedByUser: true });
    },
  };
}
