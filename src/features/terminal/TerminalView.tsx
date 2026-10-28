import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { Unicode11Addon } from "@xterm/addon-unicode11";
import { WebLinksAddon } from "@xterm/addon-web-links";
import "@xterm/xterm/css/xterm.css";
import "./terminal.css";
import { currentTerminalTheme } from "./theme";
import { needsPasteConfirm, parseOsc7, parseOsc99 } from "./util";
import { useSettings } from "../settings/store";
import { useContextMenu } from "../../components";
import { formatError } from "../../errors";
import { endedFromOpenError, type Ended } from "./ended";
import { getServersApi, notifyServersChanged } from "../servers/api";
import type { OpenSpec, TerminalBackend } from "./types";
import { Button } from "../../components";
import { readClipboardText } from "../shell/clipboard";
import { passwordBus } from "../passwords/bus";
import { KittyKeyboard, localShellFamily, newlineKey, parseOsc52 } from "./kittyKeys";

interface Props {
  backend: TerminalBackend;
  spec: OpenSpec;
  /** 当前标签是否可见（隐藏的标签保持会话，但不重新计算尺寸） */
  visible: boolean;
  /** 多行粘贴前请求确认；返回 true 才粘贴 */
  confirmPaste: (text: string) => Promise<boolean>;
  /** 点击终端里的链接时请求确认并打开 */
  openLink: (url: string) => void;
  /** 会话状态变化：运行中 / 已结束（断开、退出、打开失败） */
  onStatus?: (status: "running" | "exited") => void;
  /** 关闭这个窗格（提示块上的“关闭”按钮） */
  onCloseTab?: () => void;
  /** 提示块上关闭按钮的文字（单个窗格为“关闭标签”，分屏时为“关闭窗格”） */
  closeLabel?: string;
  /** 终端报告了当前目录（本机 shell 的提示符钩子输出 OSC 7 或 OSC 9;9） */
  onCwd?: (path: string) => void;
  /** 用户在终端里按了回车（可能执行了 cd，服务器终端据此重新查询当前目录） */
  onCommand?: () => void;
  /** 后端会话 ID 变化（打开成功、重新连接时为新 ID，结束时为 null） */
  onSession?: (id: string | null) => void;
  /** 打开失败（错误对象原样交给上层翻译） */
  onError?: (err: unknown) => void;
  /** 这个窗格正在参与广播输入（多行粘贴一律先确认） */
  broadcasting?: boolean;
  /** 广播时接管键盘输入：返回 true 表示已由上层发给各窗格，这里不再自己发送 */
  onInput?: (data: string) => boolean;
  /** 广播时把最近一段输出交给上层（用来识别密码提示） */
  onOutput?: (tail: string) => void;
  /** 按了 Ctrl+Shift+B：开关广播输入 */
  onToggleBroadcast?: () => void;
  /** 终端创建后交出操作句柄（命令片段用），销毁时交出 null */
  onHandle?: (h: TermHandle | null) => void;
}

/** 从外部操作终端：粘贴、输入都走终端自己的输入通道（广播、回车计数照常生效） */
export interface TermHandle {
  /** 按粘贴处理（括号粘贴模式下整体插入），不再弹多行确认 */
  paste: (text: string) => void;
  /** 当作用户按键输入（例如一个回车） */
  input: (data: string) => void;
  focus: () => void;
}

/**
 * 一个终端窗格：xterm.js 显示 + 后端会话。
 * 规则：Ctrl+C 永远发给程序；复制用 Ctrl+Shift+C；粘贴用 Ctrl+Shift+V 或右键；
 * 会话结束（断网、退出、登录失败）后，终端区域换成一块提示，提供“重新连接”和“关闭标签”；
 * 不自动重连，也不自动关闭标签（设计 3.9）。
 */
export function TerminalView({ backend, spec, visible, confirmPaste, openLink, onStatus, onError, onCloseTab, closeLabel, onCwd, onCommand, onSession, broadcasting, onInput, onOutput, onToggleBroadcast, onHandle }: Props) {
  const { t } = useTranslation();
  const host = useRef<HTMLDivElement>(null);
  // 回调放进 ref，避免它们变化时重建终端
  const { settings } = useSettings();
  const opts = settings.terminal;
  const cb = useRef({ confirmPaste, openLink, onStatus, onError, onCwd, onCommand, onSession, t, opts, broadcasting, onInput, onOutput, onToggleBroadcast, onHandle });
  cb.current = { confirmPaste, openLink, onStatus, onError, onCwd, onCommand, onSession, t, opts, broadcasting, onInput, onOutput, onToggleBroadcast, onHandle };
  const termRef = useRef<Terminal | null>(null);
  // 粘贴要走多行确认，函数在终端创建时生成，经 ref 给右键菜单用
  const pasteRef = useRef<(text: string) => Promise<void>>(async () => {});
  const ctx = useContextMenu(t("terminal.menuLabel"));
  const fitRef = useRef<FitAddon | null>(null);
  const resizeRef = useRef<() => void>(() => {});
  // 输入发送失败等短暂提示，显示在终端顶部，几秒后自动消失（不写进终端内容，避免打乱程序输出）
  const [notice, setNotice] = useState<string | null>(null);
  const [ended, setEnded] = useState<Ended | null>(null);
  /** 内置 SSH 连不上或密钥没通过：提供“改用系统 OpenSSH 重试”（只对保存过的服务器） */
  const offerOpenSsh = spec.kind === "server" && (ended?.kind === "failed" || ended?.kind === "authFailed") && !!ended.builtin;
  /** 把这台服务器改成系统 OpenSSH（原因写进服务器设置），再重新连接 */
  const switchToOpenSsh = async () => {
    if (spec.kind !== "server" || !ended) return;
    const reason = ended.kind === "failed" ? ended.text : t("terminal.endedAuth");
    try {
      await (await getServersApi()).setEngine(spec.serverId, "openssh", t("terminal.openSshNote", { reason }));
      notifyServersChanged();
      reopenRef.current();
    } catch (e) {
      setEnded({ kind: "failed", text: formatError(e) });
    }
  };
  // 没输密码而结束的服务器终端：这台服务器有了密码（在别处输入，或密码框晚一步答完）就自动重新连接
  useEffect(() => {
    if (ended?.kind !== "passwordNeeded" || spec.kind !== "server") return;
    const serverId = spec.serverId;
    return passwordBus.onSaved((id) => { if (id === serverId) reopenRef.current(); });
  }, [ended, spec]);
  // 提示块上的“重新连接”要调用终端创建时生成的打开函数
  const reopenRef = useRef<() => void>(() => {});
  const noticeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const showNotice = useRef((text: string, ms = 4000) => {
    setNotice(text);
    if (noticeTimer.current) clearTimeout(noticeTimer.current);
    noticeTimer.current = setTimeout(() => setNotice(null), ms);
  });

  useEffect(() => {
    const el = host.current;
    if (!el) return;
    const term = new Terminal({
      fontFamily: '"JetBrains Mono Variable", "Cascadia Mono", Consolas, "Microsoft YaHei UI", monospace',
      fontSize: opts.fontSize,
      lineHeight: 1.25,
      cursorBlink: opts.cursorBlink,
      cursorStyle: opts.cursorStyle,
      scrollback: opts.scrollback,
      allowTransparency: true,
      allowProposedApi: true,
      theme: currentTerminalTheme(),
    });
    termRef.current = term;
    const fit = new FitAddon();
    fitRef.current = fit;
    term.loadAddon(fit);
    const unicode = new Unicode11Addon();
    term.loadAddon(unicode);
    term.unicode.activeVersion = "11";
    term.loadAddon(new WebLinksAddon((_e, url) => cb.current.openLink(url)));
    term.open(el);
    fit.fit();
    // 当前目录报告：OSC 7（file://主机/路径）和 OSC 9;9（Windows Terminal 约定，"路径"）
    term.parser.registerOscHandler(7, (data) => {
      const path = parseOsc7(data);
      if (path) cb.current.onCwd?.(path);
      return true;
    });
    term.parser.registerOscHandler(9, (data) => {
      // OSC 9 还有别的用途（如通知），只处理 9;9
      if (!data.startsWith("9;")) return false;
      const path = parseOsc99(data.slice(2));
      if (path) cb.current.onCwd?.(path);
      return true;
    });
    // OSC 52：程序（Claude Code、tmux、vim 等）通过 SSH 写剪贴板；读请求一律忽略
    term.parser.registerOscHandler(52, (data) => {
      const text = parseOsc52(data);
      if (text) void navigator.clipboard.writeText(text).catch(() => undefined);
      return true;
    });
    // kitty 键盘协议（只支持“消除歧义”）：程序开启后 Shift+Enter 等能和 Enter 区分开
    const kitty = new KittyKeyboard();
    const num = (p: number | number[] | undefined, d: number) => (typeof p === "number" && p > 0 ? p : d);
    term.parser.registerCsiHandler({ prefix: ">", final: "u" }, (p) => { kitty.push(num(p[0], 0)); return true; });
    term.parser.registerCsiHandler({ prefix: "<", final: "u" }, (p) => { kitty.pop(num(p[0], 1)); return true; });
    term.parser.registerCsiHandler({ prefix: "=", final: "u" }, (p) => { kitty.set(num(p[0], 0), num(p[1], 1)); return true; });
    term.parser.registerCsiHandler({ prefix: "?", final: "u" }, () => { reply(kitty.query()); return true; });
    term.buffer.onBufferChange((b) => kitty.setScreen(b.type === "alternate"));

    let id: string | null = null;
    // 会话 ID 返回之前就到达的输出，先累计字节数，拿到 ID 后再一并确认（否则流量控制会少算）
    let earlyBytes = 0;
    let exited = false;
    let disposed = false;
    let last = { cols: term.cols, rows: term.rows };
    // 最近一段输出（只用来判断“登录失败”：ssh 打印 Permission denied 后以 255 退出）
    let tail = "";
    const decoder = new TextDecoder();

    const open = async () => {
      exited = false;
      tail = "";
      setEnded(null);
      earlyBytes = 0;
      cb.current.onStatus?.("running");
      try {
        const newId = await backend.open(spec, term.cols, term.rows, (e) => {
          if (disposed) return;
          if (e.kind === "output") {
            const n = e.data.length;
            tail = (tail + decoder.decode(e.data, { stream: true })).slice(-2048);
            cb.current.onOutput?.(tail);
            term.write(e.data, () => {
              if (id) void backend.ack(id, n);
              else earlyBytes += n;
            });
          } else {
            exited = true;
            id = null;
            cb.current.onSession?.(null);
            cb.current.onStatus?.("exited");
            if (e.closedByUser) return;
            // 后端已判断出是密码的问题
            if (e.auth === "ssh.password_needed") { setEnded({ kind: "passwordNeeded" }); return; }
            if (e.auth === "ssh.auth_failed") { setEnded({ kind: "authFailed", password: true }); return; }
            if (e.auth === "ssh.two_factor_required") { setEnded({ kind: "authFailed", twoFactor: true }); return; }
            if (e.auth === "ssh.too_many_auth_attempts") { setEnded({ kind: "authFailed", tooManyAuth: true }); return; }
            // 远程会话里 ssh 以 255 退出：连接断开（网络中断、服务器重启等），或登录失败
            if (spec.kind !== "local" && e.code === 255) {
              setEnded(/Too many authentication failures/i.test(tail) ? { kind: "authFailed", tooManyAuth: true }
                : /Permission denied/i.test(tail) ? { kind: "authFailed" } : { kind: "lost" });
            } else {
              setEnded({ kind: "exited", code: e.code ?? null });
            }
          }
        });
        if (disposed) { void backend.close(newId); return; }
        id = newId;
        cb.current.onSession?.(newId);
        if (earlyBytes > 0) {
          void backend.ack(newId, earlyBytes);
          earlyBytes = 0;
        }
      } catch (err) {
        exited = true;
        cb.current.onStatus?.("exited");
        cb.current.onError?.(err);
        setEnded(endedFromOpenError(err));
      }
    };

    // 终端对程序查询的回答：只发给这个会话（不走广播）
    const reply = (data: string) => {
      if (id && !exited) void backend.write(id, data).catch(() => undefined);
    };
    const reopen = () => { term.reset(); kitty.reset(); void open(); };
    reopenRef.current = reopen;
    term.onData((data) => {
      // 会话已结束：按 Enter 等于点“重新连接”
      if (exited) {
        if (data === "\r") reopen();
        return;
      }
      if (data.includes("\r")) cb.current.onCommand?.();
      // 广播输入：由上层发给参与广播的所有窗格（包括这一个）
      if (cb.current.onInput?.(data)) return;
      if (id) {
        backend.write(id, data).catch((err: unknown) => {
          if (!disposed) showNotice.current(formatError(err));
        });
      }
    });

    const doPaste = async (text: string) => {
      if (!text) return;
      // 广播时多行粘贴一律确认（会同时进入多个窗格），不受“多行粘贴前确认”设置影响
      const broadcast = !!cb.current.broadcasting;
      if ((broadcast || cb.current.opts.confirmMultilinePaste) && needsPasteConfirm(text, term.modes.bracketedPasteMode, broadcast) && !(await cb.current.confirmPaste(text))) return;
      term.paste(text);
    };
    pasteRef.current = doPaste;

    // 复制粘贴快捷键：Ctrl+Shift+C / Ctrl+Shift+V；Ctrl+Shift+B 开关广播输入。
    // 粘贴快捷键在设置里选：选 Ctrl+V 时 Ctrl+V 粘贴，否则发给程序；Ctrl+Shift+V 一直能粘贴；Ctrl+C 永远发给程序（中断）。
    // 拦下的按键要阻止浏览器默认动作：Ctrl+V 在浏览器里本身就是粘贴，不阻止会再粘贴一次（粘贴两份）
    term.attachCustomKeyEventHandler((ev) => {
      // Shift+Enter、Ctrl+Enter 是换行（按所在环境发能换行的按键）；其他带修饰键的回车在程序开启 kitty 协议时按协议发
      const seq = newlineKey(ev, {
        kitty: (kitty.flags & 1) !== 0,
        local: spec.kind === "local" ? localShellFamily(spec.shellId) : null,
        altScreen: term.buffer.active.type === "alternate",
      }) ?? kitty.encode(ev);
      if (seq !== null) {
        // keydown、keypress 都要拦下，否则 xterm 还会再发一个回车
        ev.preventDefault();
        if (ev.type === "keydown") term.input(seq, true);
        return false;
      }
      const ctrlOnly = ev.ctrlKey && !ev.shiftKey && !ev.altKey && !ev.metaKey;
      if (ctrlOnly && ev.code === "KeyV" && cb.current.opts.pasteKey === "ctrlV") {
        ev.preventDefault();
        if (ev.type === "keydown") void readClipboardText().then(doPaste, (e: unknown) => showNotice.current(formatError(e)));
        return false;
      }
      if (!ev.ctrlKey || !ev.shiftKey || ev.altKey || ev.metaKey) return true;
      if (ev.code !== "KeyC" && ev.code !== "KeyV" && ev.code !== "KeyB") return true;
      if (ev.code === "KeyB" && !cb.current.onToggleBroadcast) return true;
      ev.preventDefault();
      if (ev.type !== "keydown") return false;
      if (ev.code === "KeyB") {
        cb.current.onToggleBroadcast?.();
      } else if (ev.code === "KeyC") {
        const sel = term.getSelection();
        if (sel) void navigator.clipboard.writeText(sel);
      } else {
        void readClipboardText().then(doPaste, (e: unknown) => showNotice.current(formatError(e)));
      }
      return false;
    });

    // 浏览器原生粘贴（右键菜单、Shift+Insert）也走多行确认
    const onPaste = (ev: ClipboardEvent) => {
      ev.preventDefault();
      ev.stopPropagation();
      void doPaste(ev.clipboardData?.getData("text/plain") ?? "");
    };
    term.textarea?.addEventListener("paste", onPaste, true);

    // 有选中文字时右键只弹菜单：不把这次点击发给开了鼠标模式的程序（发过去会清掉选中，复制不了）
    const onRightButton = (ev: MouseEvent) => {
      if (ev.button === 2 && term.hasSelection()) ev.stopPropagation();
    };
    el.addEventListener("mousedown", onRightButton, true);
    el.addEventListener("mouseup", onRightButton, true);

    // 窗口大小变化：节流 100 毫秒，只在行列真的变化时通知后端（异常 R5）
    let timer: ReturnType<typeof setTimeout> | null = null;
    const doResize = () => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        if (disposed || !el.offsetWidth || !el.offsetHeight) return;
        fit.fit();
        if ((term.cols !== last.cols || term.rows !== last.rows) && id) {
          last = { cols: term.cols, rows: term.rows };
          void backend.resize(id, term.cols, term.rows);
        }
      }, 100);
    };
    resizeRef.current = doResize;
    const ro = new ResizeObserver(doResize);
    ro.observe(el);
    // 字体加载完成后字符尺寸会变，需要重新计算行列
    void document.fonts?.ready.then(doResize);

    // 主题切换时更新终端配色
    const mo = new MutationObserver(() => { term.options.theme = currentTerminalTheme(); });
    mo.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

    void open();
    cb.current.onHandle?.({ paste: (text) => term.paste(text), input: (data) => term.input(data, true), focus: () => term.focus() });

    return () => {
      cb.current.onHandle?.(null);
      disposed = true;
      if (timer) clearTimeout(timer);
      ro.disconnect();
      mo.disconnect();
      term.textarea?.removeEventListener("paste", onPaste, true);
      el.removeEventListener("mousedown", onRightButton, true);
      el.removeEventListener("mouseup", onRightButton, true);
      if (id) void backend.close(id);
      if (noticeTimer.current) clearTimeout(noticeTimer.current);
      termRef.current = null;
      term.dispose();
    };
    // spec 和 backend 在一个窗格的生命周期内不变
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (visible) resizeRef.current();
  }, [visible]);

  // 设置变化时更新已打开的终端（字号变化后行列会变，需要重新计算尺寸）
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.cursorStyle = opts.cursorStyle;
    term.options.cursorBlink = opts.cursorBlink;
    term.options.scrollback = opts.scrollback;
    if (term.options.fontSize !== opts.fontSize) {
      term.options.fontSize = opts.fontSize;
      resizeRef.current();
    }
  }, [opts.cursorStyle, opts.cursorBlink, opts.scrollback, opts.fontSize]);

  return (
    <div
      className="term-pane"
      style={{ display: visible ? "flex" : "none" }}
      onContextMenu={(e) => {
        const term = termRef.current;
        if (!term) return;
        const selection = term.getSelection();
        // 设置里开了“选中后右键直接复制”：复制并取消选中，不弹菜单
        if (selection && cb.current.opts.rightClickCopy) {
          e.preventDefault();
          void navigator.clipboard.writeText(selection);
          term.clearSelection();
          term.focus();
          return;
        }
        // 设置里开了“没有选中时右键直接粘贴”：粘贴（多行照常确认），不弹菜单
        if (!selection && cb.current.opts.rightClickPaste) {
          e.preventDefault();
          void readClipboardText().then((text) => pasteRef.current(text), (err: unknown) => showNotice.current(formatError(err))).finally(() => term.focus());
          return;
        }
        ctx.open(e, [
          { id: "copy", label: t("terminal.copy"), shortcut: "Ctrl Shift C", disabled: !selection, onSelect: () => { void navigator.clipboard.writeText(selection); term.focus(); } },
          { id: "paste", label: t("terminal.paste"), shortcut: cb.current.opts.pasteKey === "ctrlV" ? "Ctrl V" : "Ctrl Shift V", onSelect: () => { void readClipboardText().then((text) => pasteRef.current(text), (e: unknown) => showNotice.current(formatError(e))).finally(() => term.focus()); } },
          { kind: "separator", id: "s1" },
          { id: "all", label: t("terminal.selectAll"), onSelect: () => term.selectAll() },
          { id: "clear", label: t("terminal.clear"), onSelect: () => { term.clear(); term.focus(); } },
        ]);
      }}
    >
      <div ref={host} className="term-host" />
      {ended && (
        // 盖住终端内容：断开后终端里不再显示任何东西，只显示状态和两个按钮
        <div className={`term-ended term-ended--${ended.kind}`} role="alert">
          <div className="term-ended__box">
            <div className="term-ended__title">
              {ended.kind === "lost" ? t("terminal.endedLost")
                : ended.kind === "passwordNeeded" ? t("terminal.endedPasswordNeeded")
                : ended.kind === "authFailed" ? t("terminal.endedAuth")
                : ended.kind === "failed" ? t("terminal.endedFailed")
                : t("terminal.exited", { code: ended.code ?? "-" })}
            </div>
            <div className="term-ended__detail">
              {ended.kind === "lost" ? t("terminal.endedLostDetail")
                : ended.kind === "passwordNeeded" ? t("terminal.endedPasswordNeededDetail")
                : ended.kind === "authFailed" ? t(ended.tooManyAuth ? "terminal.endedTooManyAuthDetail" : ended.twoFactor ? "terminal.endedTwoFactorDetail" : ended.password ? "terminal.endedAuthPasswordDetail" : "terminal.endedAuthDetail")
                : ended.kind === "failed" ? ended.text
                : t("terminal.endedExitedDetail")}
            </div>
            {offerOpenSsh && <div className="term-ended__detail">{t("terminal.openSshOffer")}</div>}
            <div className="term-ended__actions">
              <Button onClick={() => onCloseTab?.()}>{closeLabel ?? t("terminal.closeTab")}</Button>
              {offerOpenSsh && <Button className="term-ended__openssh" onClick={() => void switchToOpenSsh()}>{t("terminal.useOpenSsh")}</Button>}
              <Button variant="primary" autoFocus onClick={() => reopenRef.current()}>
                {spec.kind === "local" ? t("terminal.reopen") : t("terminal.reconnect")}
              </Button>
            </div>
            <div className="term-ended__hint">{t("terminal.endedEnterHint")}</div>
          </div>
        </div>
      )}
      {notice && (
        <div className="term-notice" role="status">
          {notice}
        </div>
      )}
      {ctx.node}
    </div>
  );
}
