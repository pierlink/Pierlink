/// <reference types="node" />
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import zh from "./zh-CN.json";
import en from "./en.json";

/** 早先就没写提示的错误码（多为内部错误或只返回给 AI 的），以后逐步补上；新错误码不要加进来 */
const KNOWN_MISSING = new Set([
  // 只在真实服务器验收测试里用
  "ssh.invalid_parameter",
  "test.desktop_process_missing",
  "test.invalid_process_list",
  "deploy.not_found",
  "detect.shell_config_failed",
  "keys.invalid_password_command",
  "keys.invalid_verify_command",
  "mcp.action_denied",
  "mcp.audit_failed",
  "mcp.busy",
  "mcp.cancelled",
  "mcp.commands_denied",
  "mcp.connection_denied",
  "mcp.cwd_not_found",
  "mcp.file_too_large",
  "mcp.invalid_argument",
  "mcp.invalid_cwd",
  "mcp.not_found",
  "mcp.password_prompt",
  "mcp.path_denied",
  "mcp.terminal_close_denied",
  "mcp.terminal_not_handed_over",
  "mcp.terminal_registered",
  "mobile.host_unavailable",
  "mobile.invalid_request",
  "mobile.password_prompt",
  "monitor.invalid_interval",
  "monitor.no_posix_shell",
  "monitor.sample_timeout",
  "monitor.task_failed",
  "ssh.askpass_invalid_path",
  "ssh.askpass_needs_resolved_target",
  "ssh.askpass_store_failed",
  "ssh.background_command_required",
  "ssh.conversion_not_imported",
  "ssh.host_key_probe_required",
  "ssh.invalid_host",
  "ssh.invalid_host_key",
  "ssh.invalid_isolated_config",
  "ssh.jump_alias_needs_resolved",
  "ssh.jump_cycle",
  "ssh.jump_lookup_failed",
  "ssh.jump_not_found",
  "ssh.jump_too_deep",
  "ssh.known_hosts_failed",
  "ssh.known_hosts_required",
  "storage.json",
  "sync.estimate_file_changed",
  "sync.estimate_transport_missing",
  "sync.folder_not_found",
  "sync.invalid_mode",
  "sync.rules_update_failed",
  "sync.server_offline",
  "syncthing.adopt_failed",
  "syncthing.forbidden_change",
  "syncthing.invalid_config",
  "syncthing.invalid_response",
  "syncthing.io_failed",
  "syncthing.listen_address_mismatch",
  "syncthing.response_too_large",
  "syncthing.socket_dir_insecure",
  "syncthing.socket_path_too_long",
  "syncthing.timeout",
  "syncthing.versions_contains_folder",
  "syncthing.versions_inside_folder",
  "terminal.page_reloaded",
  "test.cancelled",
  "test.home_changed",
  "transfer.non_atomic_overwrite",
  "tunnel.already_exists",
  "tunnel.duplicate_port",
  "tunnel.invalid_port",
  "tunnel.invalid_spec",
  "tunnel.limit_reached",
  "tunnel.not_found",
  "tunnel.test_failure",
]);

function rustFiles(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    if (name === "target" || name === "node_modules") return [];
    return statSync(p).isDirectory() ? rustFiles(p) : p.endsWith(".rs") ? [p] : [];
  });
}

function files(dir: string, ext: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const p = join(dir, name);
    if (name === "target" || name === "node_modules") return [];
    return statSync(p).isDirectory() ? files(p, ext) : p.endsWith(ext) ? [p] : [];
  });
}

function codes(): string[] {
  const all = new Set<string>();
  for (const f of [...rustFiles("crates"), ...rustFiles("src-tauri/src")]) {
    for (const m of readFileSync(f, "utf8").matchAll(/AppError::new\(\s*"([a-z_]+\.[a-z0-9_]+)"/g)) all.add(m[1]);
  }
  // 桌面模块用 error("代码") 和服务器脚本里的 fail 代码 生成 vnc.* 错误
  for (const f of files("crates/core/src/vnc", ".rs")) {
    for (const m of readFileSync(f, "utf8").matchAll(/\berror\("([a-z_]+)"\)/g)) all.add(`vnc.${m[1]}`);
  }
  for (const f of files("crates/core/src/vnc", ".sh")) {
    for (const m of readFileSync(f, "utf8").matchAll(/\bfail ([a-z_]+)/g)) all.add(`vnc.${m[1]}`);
    for (const m of readFileSync(f, "utf8").matchAll(/"error":"(vnc\.[a-z_]+)"/g)) all.add(m[1]);
  }
  return [...all].sort();
}

function has(locale: Record<string, unknown>, code: string): boolean {
  const [ns, key] = code.split(".");
  const errors = locale.errors as Record<string, Record<string, unknown> | undefined>;
  return typeof errors[ns]?.[key] === "string";
}

describe("错误提示", () => {
  it("后端的每个错误码都有中英文提示", () => {
    const list = codes();
    expect(list.length).toBeGreaterThan(100);
    const locales: [string, Record<string, unknown>][] = [["zh-CN", zh], ["en", en]];
    for (const [name, locale] of locales) {
      const missing = list.filter((c) => !KNOWN_MISSING.has(c) && !has(locale, c));
      expect(missing, name).toEqual([]);
    }
  });
});
