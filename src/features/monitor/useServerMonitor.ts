import { useEffect, useRef, useState } from "react";
import { formatError, isAppError } from "../../errors";
import { getMonitorApi, type MonitorEvent, type MonitorWatch, type ResourceSample } from "./api";
import { loginServer, needsLogin, onConnectionState } from "../servers/login";
import { getServersApi, onServersChanged } from "../servers/api";

/** 概况的连接状态 */
export type MonitorState =
  | { kind: "waiting" }
  | { kind: "connecting" }
  | { kind: "ok" }
  | { kind: "retry"; text: string }
  | { kind: "unsupported" }
  | { kind: "failed"; text: string }
  /** 等用户输入密码（输了之后后端自动接着采样） */
  | { kind: "password"; wrong: boolean }
  /** 要登录（验证码），或密钥太多撞上登录上限（reason 是完整说明） */
  | { kind: "login"; busy: boolean; error: string | null; reason?: string };

/**
 * 看一台服务器的概况。同一台服务器在应用里只采一份（后端共用），
 * visible 为 false 时只是隐藏：采样放慢但不停，再显示时马上有数。
 *
 * @param serverId 服务器 ID；为 null 时不看
 * @param visible 概况是否显示
 * @returns 最近一帧、状态、登录函数
 */
export function useServerMonitor(serverId: string | null, visible: boolean) {
  const [sample, setSample] = useState<ResourceSample | null>(null);
  const [state, setState] = useState<MonitorState>({ kind: "waiting" });
  // 要验证码的服务器（内置 SSH）：登录一次后重新开始采样；换了连接方式也重新开始
  const [restart, setRestart] = useState(0);
  const waitingLogin = useRef(false);
  waitingLogin.current = state.kind === "login";
  const watch = useRef<MonitorWatch | null>(null);
  const visibleRef = useRef(visible);
  visibleRef.current = visible;

  const engine = useRef<string | undefined>(undefined);
  useEffect(() => {
    if (!serverId) return;
    engine.current = undefined;
    const check = () => void getServersApi().then((api) => api.list()).then((list) => {
      const now = list.find((x) => x.id === serverId)?.connectionEngine;
      if (engine.current !== undefined && now !== engine.current) setRestart((n) => n + 1);
      engine.current = now;
    }, () => { /* 读不到列表时不处理 */ });
    check();
    return onServersChanged(check);
  }, [serverId]);

  useEffect(() => onConnectionState((e) => {
    if (e.server_id === serverId && e.state === "connected" && waitingLogin.current) {
      waitingLogin.current = false;
      setRestart((n) => n + 1);
    }
  }), [serverId]);

  const login = () => {
    if (!serverId) return;
    setState((s) => ({ kind: "login", busy: true, error: null, reason: s.kind === "login" ? s.reason : undefined }));
    loginServer(serverId).then(
      () => { if (waitingLogin.current) { waitingLogin.current = false; setRestart((n) => n + 1); } },
      (e: unknown) => setState((s) => ({ kind: "login", busy: false, error: formatError(e), reason: s.kind === "login" ? s.reason : undefined })),
    );
  };

  useEffect(() => {
    setSample(null);
    setState({ kind: "waiting" });
    if (!serverId) return;
    let alive = true;
    const onEvent = (e: MonitorEvent) => {
      if (!alive) return;
      if (e.kind === "connecting") setState((s) => (s.kind === "ok" ? s : { kind: "connecting" }));
      // 系统名只在每次连接的第一次采样里带，之后的采样沿用上一次的
      else if (e.kind === "sample") { setSample((prev) => ({ ...e.sample, osName: e.sample.osName ?? prev?.osName ?? null })); setState({ kind: "ok" }); }
      else if (e.kind === "reconnecting") setState({ kind: "retry", text: formatError(e.error) });
      else if (e.kind === "waitingPassword") setState({ kind: "password", wrong: isAppError(e.error) && e.error.code === "ssh.auth_failed" });
      else if (e.kind === "unsupported") setState({ kind: "unsupported" });
      else if (e.kind === "failed") {
        const tooManyAuth = isAppError(e.error) && e.error.code === "ssh.too_many_auth_attempts" && e.error.params?.engine === "builtin";
        setState(needsLogin(e.error) ? { kind: "login", busy: false, error: null }
          : tooManyAuth ? { kind: "login", busy: false, error: null, reason: formatError(e.error) }
          : { kind: "failed", text: formatError(e.error) });
      }
    };
    void getMonitorApi().then((api) => api.watch(serverId, onEvent, visibleRef.current)).then((w) => {
      if (alive) watch.current = w; else w.stop();
    }, (e: unknown) => { if (alive) setState({ kind: "failed", text: formatError(e) }); });
    return () => { alive = false; watch.current?.stop(); watch.current = null; };
  }, [serverId, restart]);

  useEffect(() => { watch.current?.setVisible(visible); }, [visible]);

  return { sample, state, login };
}
