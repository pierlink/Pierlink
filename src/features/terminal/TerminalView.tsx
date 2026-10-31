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
import { ConnectProgress, ConnectSteps, stageIndex, type Step } from "./ConnectProgress";
import { findFakeCursor } from "./imeAnchor";
import { useSettings } from "../settings/store";
import { useContextMenu } from "../../components";
import { formatError, isAppError } from "../../errors";
import { endedFromOpenError, endedLine, endsInline, type Ended } from "./ended";
import { getServersApi, notifyServersChanged } from "../servers/api";
import type { OpenSpec, TerminalBackend } from "./types";
import { Button } from "../../components";
import { readClipboardText, writeClipboardText } from "../shell/clipboard";
import { commands } from "../../bindings/commands";
import { inTauri } from "../shell/TitleBar";
import { explorerFolder } from "./localPaths";
import { passwordBus } from "../passwords/bus";
import { KittyKeyboard, localShellFamily, newlineKey, parseOsc52 } from "./kittyKeys";
import { Prediction } from "./prediction";
import { historyScope } from "./predict";

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
  /** 终端当前目录（不知道时为 null）：右键菜单“复制当前目录”“在资源管理器中打开”用 */
  cwd?: string | null;
  /** 右键菜单的“左右分屏”“上下分屏”：在旁边开一个同样的终端 */
  onSplit?: (dir: "row" | "col") => void;
  /** 窗格太小、拆开后会小于最小尺寸时返回 false，菜单项灰掉 */
  canSplit?: (dir: "row" | "col") => boolean;
  /** 正从资源管理器拖着文件经过这个本机终端：显示“松开填入路径” */
  fileDrop?: boolean;
  /** 显示命令预测（设置开着、这台服务器没关、没交给 AI） */
  prediction?: boolean;
}

/** 从外部操作终端：粘贴、输入都走终端自己的输入通道（广播、回车计数照常生效） */
export interface TermHandle {
  /** 按粘贴处理（括号粘贴模式下整体插入），不再弹多行确认 */
  paste: (text: string) => void;
  /** 当作用户按键输入（例如一个回车） */
  input: (data: string) => void;
  focus: () => void;
  /** 断开会话（服务器终端的“断开”按钮）：内容留在终端里，最下面写一行“已断开” */
  disconnect: () => void;
  /** 会话已结束时重新连接（清屏后重新打开，和按 Enter 一样）；还连着时不做什么 */
  reconnect: () => void;
}

/**
 * 一个终端窗格：xterm.js 显示 + 后端会话。
 * 规则：有选中文字时 Ctrl+C、Ctrl+Shift+C 复制，没有时 Ctrl+C 发给程序（中断）；Ctrl+V、Ctrl+Shift+V 粘贴；
 * 会话断开或退出后，终端内容留着，最下面写一行说明，按 Enter 重新连接；登录失败、没能打开时盖上一块提示，
 * 提供“重新连接”和“关闭标签”。不自动重连，也不自动关闭标签（设计 3.9）。
 */
export function TerminalView({ backend, spec, visible, confirmPaste, openLink, onStatus, onError, onCloseTab, closeLabel, onCwd, onCommand, onSession, broadcasting, onInput, onOutput, onToggleBroadcast, onHandle, cwd, onSplit, canSplit, fileDrop, prediction }: Props) {
  const { t } = useTranslation();
  const host = useRef<HTMLDivElement>(null);
  // 回调放进 ref，避免它们变化时重建终端
  const { settings } = useSettings();
  const opts = settings.terminal;
  const cb = useRef({ confirmPaste, openLink, onStatus, onError, onCwd, onCommand, onSession, t, opts, broadcasting, onInput, onOutput, onToggleBroadcast, onHandle, cwd, onSplit, canSplit, prediction });
  cb.current = { confirmPaste, openLink, onStatus, onError, onCwd, onCommand, onSession, t, opts, broadcasting, onInput, onOutput, onToggleBroadcast, onHandle, cwd, onSplit, canSplit, prediction };
  const predictionRef = useRef<Prediction | null>(null);
  const termRef = useRef<Terminal | null>(null);
  // 粘贴要走多行确认，函数在终端创建时生成，经 ref 给右键菜单用
  const pasteRef = useRef<(text: string) => Promise<void>>(async () => {});
  const ctx = useContextMenu(t("terminal.menuLabel"));
  const fitRef = useRef<FitAddon | null>(null);
  const resizeRef = useRef<() => void>(() => {});
  // 输入发送失败等短暂提示，显示在终端顶部，几秒后自动消失（不写进终端内容，避免打乱程序输出）
  const [notice, setNotice] = useState<string | null>(null);
  const [ended, setEnded] = useState<Ended | null>(null);
  /** 连接服务器的进度：当前一步和开始时间；failed 表示连接停在这一步（显示在失败提示块上） */
  const [steps, setSteps] = useState<{ stage: Step; start: number; failed: boolean } | null>(null);
  /** 内置 SSH 连不上或密钥没通过：提供“改用系统 OpenSSH 重试”（只对保存过的服务器） */
  const offerOpenSsh = spec.kind === "server" && (ended?.kind === "failed" || ended?.kind === "authFailed") && !!ended.builtin;
  /** 把这台服务器改成系统 OpenSSH（原因写进服务器设置），再重新连接 */
  const switchToOpenSsh = async () => {
    if (spec.kind !== "server" || !ended) return;
    const reason = ended.kind === "failed" ? ended.text : t("terminal.endedAuth");
    try {
      await (await getServersApi()).setEngine(spec.serverId, "openssh", t("terminal.openSshNote", { reason }));
      notifyServersChanged();
      reopenRef.current(true);
    } catch (e) {
      setSteps(null);
      setEnded({ kind: "failed", text: formatError(e) });
    }
  };
  // 没输密码而结束的服务器终端：这台服务器有了密码（在别处输入，或密码框晚一步答完）就自动重新连接
  useEffect(() => {
    if (ended?.kind !== "passwordNeeded" || spec.kind !== "server") return;
    const serverId = spec.serverId;
    return passwordBus.onSaved((id) => { if (id === serverId) reopenRef.current(); });
  }, [ended, spec]);
  // 提示块上的“重新连接”要调用终端创建时生成的打开函数；focus：点按钮重新连接时把焦点放回终端
  const reopenRef = useRef<(focus?: boolean) => void>(() => {});
  const noticeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const showNotice = useRef((text: string, ms = 4000) => {
    setNotice(text);
    if (noticeTimer.current) clearTimeout(noticeTimer.current);
    noticeTimer.current = setTimeout(() => setNotice(null), ms);
  });
  /** 复制到系统剪贴板；失败时在终端顶部提示原因 */
  const copyText = useRef((text: string) => {
    if (text) void writeClipboardText(text).catch((e: unknown) => showNotice.current(formatError(e)));
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
      cursorWidth: opts.cursorWidth,
      scrollback: opts.scrollback,
      allowTransparency: true,
      allowProposedApi: true,
      theme: currentTerminalTheme(),
      // 程序输出的超链接（OSC 8，如 Claude Code 里的链接）：和普通网址一样先确认再用系统浏览器打开；拖选链接文字时不打开
      linkHandler: { activate: (_e, uri) => { if (!termRef.current?.hasSelection()) cb.current.openLink(uri); } },
    });
    termRef.current = term;
    const fit = new FitAddon();
    fitRef.current = fit;
    term.loadAddon(fit);
    const unicode = new Unicode11Addon();
    term.loadAddon(unicode);
    term.unicode.activeVersion = "11";
    // 按下和松开都在同一个链接上就会触发，拖选链接里的一段文字时也一样：有选中内容说明是在选文字，不打开
    term.loadAddon(new WebLinksAddon((_e, url) => { if (!term.hasSelection()) cb.current.openLink(url); }));
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
    // OSC 52：程序（Claude Code、tmux、vim 等）通过 SSH 写剪贴板；读请求一律忽略。
    // 由后端写系统剪贴板：网页方式要求窗口在前台，窗口不在前台时会写不进去。
    // 注意：GNU screen 4.x 会丢掉程序直接发的 OSC 52（包在 DCS 里的才透传），tmux 会转出来
    term.parser.registerOscHandler(52, (data) => {
      const text = parseOsc52(data);
      if (text) copyText.current(text);
      return true;
    });
    // 命令预测：听 shell 钩子的提示符标记，在光标后面用浅色画出预测
    const prediction = new Prediction(term, {
      scope: historyScope(spec),
      enabled: () => !!cb.current.prediction && !cb.current.broadcasting,
      record: () => !!cb.current.prediction,
      cwd: () => cb.current.cwd ?? null,
    });
    predictionRef.current = prediction;
    // kitty 键盘协议（只支持“消除歧义”）：程序开启后 Shift+Enter 等能和 Enter 区分开
    const kitty = new KittyKeyboard();
    const num = (p: number | number[] | undefined, d: number) => (typeof p === "number" && p > 0 ? p : d);
    term.parser.registerCsiHandler({ prefix: ">", final: "u" }, (p) => { kitty.push(num(p[0], 0)); return true; });
    term.parser.registerCsiHandler({ prefix: "<", final: "u" }, (p) => { kitty.pop(num(p[0], 1)); return true; });
    term.parser.registerCsiHandler({ prefix: "=", final: "u" }, (p) => { kitty.set(num(p[0], 0), num(p[1], 1)); return true; });
    term.parser.registerCsiHandler({ prefix: "?", final: "u" }, () => { reply(kitty.query()); return true; });
    term.buffer.onBufferChange((b) => kitty.setScreen(b.type === "alternate"));
    // 程序有没有把光标藏起来、自己画光标（Claude Code 这类 CLI）：粘贴时据此判断前台是程序。只记录，照常交给 xterm 处理
    let cursorHidden = false;
    const watchCursor = (show: boolean) => (params: (number | number[])[]) => {
      if (params.includes(25)) cursorHidden = !show;
      return false;
    };
    term.parser.registerCsiHandler({ prefix: "?", final: "h" }, watchCursor(true));
    term.parser.registerCsiHandler({ prefix: "?", final: "l" }, watchCursor(false));
    // 软复位会把光标显示出来
    term.parser.registerCsiHandler({ intermediates: "!", final: "p" }, () => { cursorHidden = false; return false; });
    /**
     * 前台是程序而不是 shell 提示符（粘贴多行时据此决定要不要提醒）。装了提示符钩子时只信钩子：
     * 有命令在运行，并且不是 ssh、sudo -i、tmux 这类会进到另一个 shell 的命令。没装钩子时，主屏上藏了光标
     * 自己画、或开了鼠标上报的程序才算（如 Claude Code）；全屏的 vim、tmux、screen 不算，仍按 shell 提醒。
     */
    const inProgram = () =>
      prediction.integrated
        ? prediction.programRunning
        : term.buffer.active.type !== "alternate" && (cursorHidden || term.modes.mouseTrackingMode !== "none");

    // 输入法：程序藏了真实光标、自己画一个反色格当光标（Claude Code 等）时，组字框和候选框放到这一格。
    // xterm 按真实光标摆放它们；Windows 自带的 ConPTY 在光标隐藏时不报光标位置，真实光标会停在别处，
    // 拼音就画到了另一行。只在屏幕上恰好有一个单独的反色格时才挪，否则照 xterm 原样。
    let want: { left: string; top: string } | null = null;
    const applyIme = () => {
      if (!want) return;
      for (const node of [term.textarea, el.querySelector<HTMLElement>(".composition-view")]) {
        if (node && (node.style.left !== want.left || node.style.top !== want.top)) {
          node.style.left = want.left;
          node.style.top = want.top;
        }
      }
    };
    const anchorIme = () => {
      want = null;
      const area = term.textarea;
      if (!cursorHidden || !area || document.activeElement !== area) return;
      const at = findFakeCursor(term.buffer.active, term.cols, term.rows);
      const screen = el.querySelector<HTMLElement>(".xterm-screen");
      if (!at || !screen) return;
      want = { left: `${(at.x * screen.clientWidth) / term.cols}px`, top: `${(at.y * screen.clientHeight) / term.rows}px` };
      applyIme();
    };
    // xterm 在渲染、光标移动、组字更新时会把它们放回真实光标处（组字时还会在下一轮事件循环再放一次）：
    // 渲染、光标移动时紧跟着重新找；它一改这两个元素的位置，就在同一轮任务里改回来，不会画出错位的一帧
    const keepIme = new MutationObserver(applyIme);
    for (const node of [term.textarea, el.querySelector<HTMLElement>(".composition-view")]) {
      if (node) keepIme.observe(node, { attributes: true, attributeFilter: ["style"] });
    }
    term.onRender(anchorIme);
    term.onCursorMove(anchorIme);
    const imeEvents = ["focus", "compositionstart", "compositionupdate"] as const;
    for (const name of imeEvents) term.textarea?.addEventListener(name, anchorIme);
    const dropIme = () => { want = null; };
    term.textarea?.addEventListener("blur", dropIme);

    let id: string | null = null;
    // 会话 ID 返回之前就到达的输出，先累计字节数，拿到 ID 后再一并确认（否则流量控制会少算）
    let earlyBytes = 0;
    let exited = false;
    let disposed = false;
    // 这次连接走到的最后一步（会话可以用了以后为 null）；连接中进度还盖在终端上时 covering 为 true；
    // stageSeen：收到过后端报的阶段（系统 OpenSSH 建不了日志时一个都不报，失败时就不标停在哪一步）
    let lastStage: Step | null = null;
    let covering = false;
    let stageSeen = false;
    // 用户点了“断开”：结束后写“已断开”，不当成断网；连接过程中点的，连上后马上关掉
    let manual = false;
    // 这次连接收到多少输出、按了几次回车（带钩子连上就退出时判断是不是钩子的问题）
    let sessionBytes = 0;
    let sessionEnters = 0;
    let last = { cols: term.cols, rows: term.rows };
    // 最近一段输出（只用来判断“登录失败”：ssh 打印 Permission denied 后以 255 退出）
    let tail = "";
    const decoder = new TextDecoder();

    /** 会话结束：断开、退出时写一行说明，其他情况盖上提示块 */
    const end = (e: Ended) => {
      prediction.reset();
      kitty.reset();
      covering = false;
      // 连接没成功（还没到会话可用）：失败提示块上标出停在哪一步
      setSteps(lastStage && stageSeen && (e.kind === "failed" || e.kind === "authFailed") ? { stage: lastStage, start: 0, failed: true } : null);
      lastStage = null;
      setEnded(e);
      if (!endsInline(e)) return;
      const tr = cb.current.t;
      // 服务器拒绝了带钩子的请求时进程没启动过，不说“进程已退出”
      let text = e.kind === "lost" ? tr("terminal.lineLost")
        : e.kind === "disconnected" || (e.kind === "exited" && e.hookHint && e.code === null) ? tr("terminal.lineDisconnected")
        : tr(spec.kind === "local" ? "terminal.lineExitedLocal" : "terminal.lineExited", { code: e.kind === "exited" ? e.code ?? "-" : "-" });
      if (e.kind === "exited" && e.hookHint) text += `\r\n${tr(e.hookHint === "off" ? "terminal.hookOff" : "terminal.hookHintQuick")}`;
      // 等已收到的输出处理完再看是不是在全屏程序里；这期间重新连接了就不写
      term.write("", () => {
        if (disposed || !exited) return;
        term.write(endedLine(text, e.kind === "lost", term.buffer.active.type === "alternate"));
      });
    };

    const open = async () => {
      exited = false;
      manual = false;
      sessionBytes = 0;
      sessionEnters = 0;
      tail = "";
      setEnded(null);
      earlyBytes = 0;
      lastStage = spec.kind === "local" ? null : "resolve";
      covering = lastStage !== null;
      stageSeen = false;
      delete el.dataset.stage;
      setSteps(lastStage ? { stage: lastStage, start: performance.now(), failed: false } : null);
      cb.current.onStatus?.("running");
      try {
        const newId = await backend.open(spec, term.cols, term.rows, (e) => {
          if (disposed) return;
          if (e.kind === "stage") {
            if (!lastStage) return;
            // 走到哪一步也写在宿主元素上（端到端测试据此确认后端的阶段送到了界面）
            el.dataset.stage = e.stage;
            // 秒数从第一次报阶段算起：密码框（在连接前弹）开着的时间不算
            const first = !stageSeen;
            stageSeen = true;
            if (e.stage === "ready") {
              lastStage = null;
              covering = false;
              setSteps(null);
              return;
            }
            if (!first && stageIndex(e.stage) <= stageIndex(lastStage)) return;
            const next: Step = e.stage;
            lastStage = next;
            if (covering) setSteps((cur) => (cur && !cur.failed ? { ...cur, stage: next, start: first ? performance.now() : cur.start } : cur));
            return;
          }
          if (e.kind === "output") {
            // 终端里有了输出（密码提示、警告等）：进度让开，不挡住内容；停在哪一步仍记着，失败时用
            if (covering) {
              covering = false;
              setSteps(null);
            }
            const n = e.data.length;
            sessionBytes += n;
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
            if (e.closedByUser) { if (manual) end({ kind: "disconnected" }); return; }
            // 后端已判断出是密码的问题
            if (e.auth === "ssh.password_needed") { end({ kind: "passwordNeeded" }); return; }
            if (e.auth === "ssh.auth_failed") { end({ kind: "authFailed", password: true }); return; }
            if (e.auth === "ssh.two_factor_required") { end({ kind: "authFailed", twoFactor: true }); return; }
            if (e.auth === "ssh.too_many_auth_attempts") { end({ kind: "authFailed", tooManyAuth: true }); return; }
            // 这次实际装没装钩子由后端说了算（Windows 服务器、配置了 RemoteCommand 的服务器不装）
            const hooked = spec.kind !== "local" && !!e.hooked;
            // 服务器拒绝了带钩子的命令（系统 OpenSSH 打印 exec request failed on channel 0 后以 255 退出）
            if (e.code === 255 && hooked && !prediction.integrated && /(exec|shell) request failed/i.test(tail)) {
              hookIncompatible();
              return;
            }
            // 远程会话里 ssh 以 255 退出：连接断开（网络中断、服务器重启等），或登录失败
            if (spec.kind !== "local" && e.code === 255) {
              end(/Too many authentication failures/i.test(tail) ? { kind: "authFailed", tooManyAuth: true }
                : /Permission denied/i.test(tail) ? { kind: "authFailed" } : { kind: "lost" });
            } else {
              // 带钩子连上，一个提示符都没出现、没输出多少、也没敲几条命令（输密码最多算两次回车）就退出：
              // 多半是钩子和这台服务器不兼容（系统 OpenSSH 连的 Windows 服务器、rbash、ForceCommand 等）。
              // 保存过的服务器自动为它关掉预测（这样的服务器上预测本来也用不了），按 Enter 就按原来的方式连接
              // 只认正常的退出码：Ctrl+C 打断 ssh.exe 得到的是 Windows 的状态码（如 0xC000013A），被信号结束时为空
              const normalExit = e.code !== null && e.code !== undefined && e.code >= 0 && e.code <= 255;
              const suspect = hooked && !prediction.integrated && normalExit && sessionBytes < 8192 && sessionEnters <= 2;
              if (suspect) hookIncompatible(e.code ?? null);
              else end({ kind: "exited", code: e.code ?? null });
            }
          }
        }, !!cb.current.prediction);
        if (disposed) { void backend.close(newId); return; }
        // 已经结束（连上前就断了，退出事件先到）：不再记这个会话
        if (exited) return;
        id = newId;
        cb.current.onSession?.(newId);
        if (earlyBytes > 0) {
          void backend.ack(newId, earlyBytes);
          earlyBytes = 0;
        }
        // 连接过程中点了“断开”
        if (manual) void backend.close(newId).catch(() => undefined);
      } catch (err) {
        exited = true;
        cb.current.onStatus?.("exited");
        // 连接过程中点了“断开”，之后连接失败：照样显示“已断开”
        if (manual) { end({ kind: "disconnected" }); return; }
        // 内置 SSH：服务器拒绝了带钩子的启动请求（PTY 被拒等其他请求照常报错）
        if (isAppError(err) && err.code === "ssh.failed" && err.params?.reason_key === "hook_rejected") {
          hookIncompatible();
          return;
        }
        cb.current.onError?.(err);
        end(endedFromOpenError(err));
      }
    };
    /** 钩子和这台服务器不兼容：保存过的服务器自动关掉预测，结束行说明；快速连接提示到设置里关 */
    const hookIncompatible = (code: number | null = null) => {
      if (spec.kind === "server") void turnOffPrediction(spec.serverId);
      end({ kind: "exited", code, hookHint: spec.kind === "server" ? "off" : "quick" });
    };
    /** 为这台服务器关掉命令预测（钩子和它不兼容时）；失败时在终端顶部提示 */
    const turnOffPrediction = async (serverId: string) => {
      try {
        await (await getServersApi()).setCommandPrediction(serverId, false);
        notifyServersChanged();
      } catch (err) {
        if (!disposed) showNotice.current(formatError(err));
      }
    };
    /** 断开：已经结束时不做什么 */
    const disconnect = () => {
      if (exited || manual) return;
      manual = true;
      if (id) void backend.close(id).catch((err: unknown) => { if (!disposed) showNotice.current(formatError(err)); });
    };

    // 终端对程序查询的回答：只发给这个会话（不走广播）
    const reply = (data: string) => {
      if (id && !exited) void backend.write(id, data).catch(() => undefined);
    };
    // 结束时把光标藏了，term.reset() 又不会把它显示回来（xterm 的 reset 不清光标隐藏状态），这里显示回来
    const reopen = (focus = false) => {
      term.reset();
      term.write("\x1b[?25h");
      kitty.reset();
      prediction.reset();
      if (focus) term.focus();
      void open();
    };
    reopenRef.current = reopen;
    term.onData((data) => {
      prediction.onUserData(data);
      // 会话已结束：按 Enter 等于点“重新连接”
      if (exited) {
        if (data === "\r") reopen();
        return;
      }
      if (data.includes("\r")) {
        sessionEnters++;
        cb.current.onCommand?.();
      }
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
      const where = { bracketed: term.modes.bracketedPasteMode, broadcast, program: inProgram() };
      if ((broadcast || cb.current.opts.confirmMultilinePaste) && needsPasteConfirm(text, where) && !(await cb.current.confirmPaste(text))) return;
      term.paste(text);
    };
    pasteRef.current = doPaste;

    // 复制粘贴快捷键：有选中文字时 Ctrl+C、Ctrl+Shift+C 复制（和 Windows Terminal 一样复制后取消选中），
    // 没有选中时 Ctrl+C 照常发给程序（中断）；Ctrl+V、Ctrl+Shift+V 粘贴；Ctrl+Shift+B 开关广播输入。
    // 拦下的按键要阻止浏览器默认动作：Ctrl+V 在浏览器里本身就是粘贴，不阻止会再粘贴一次（粘贴两份）
    term.attachCustomKeyEventHandler((ev) => {
      // 会话结束后 Tab 交给浏览器：焦点能移到右下角的“关闭”“重新连接”
      if (exited && ev.key === "Tab") return false;
      // 正在显示命令预测时，→ 补全整条、Ctrl+→ 补一个词
      const predicted = prediction.handleKey(ev);
      if (predicted !== null) return predicted;
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
      if (!ev.ctrlKey || ev.altKey || ev.metaKey) return true;
      if (ev.code !== "KeyC" && ev.code !== "KeyV" && ev.code !== "KeyB") return true;
      // 只按 Ctrl+C、没有选中文字：交给 xterm 发给程序
      if (ev.code === "KeyC" && !ev.shiftKey && !term.hasSelection()) return true;
      if (ev.code === "KeyB" && (!ev.shiftKey || !cb.current.onToggleBroadcast)) return true;
      ev.preventDefault();
      if (ev.type !== "keydown") return false;
      if (ev.code === "KeyB") {
        cb.current.onToggleBroadcast?.();
      } else if (ev.code === "KeyC") {
        const sel = term.getSelection();
        if (sel) {
          copyText.current(sel);
          term.clearSelection();
        }
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
    cb.current.onHandle?.({
      paste: (text) => term.paste(text),
      input: (data) => term.input(data, true),
      focus: () => term.focus(),
      disconnect,
      reconnect: () => { if (exited) reopen(true); },
    });

    return () => {
      cb.current.onHandle?.(null);
      disposed = true;
      if (timer) clearTimeout(timer);
      ro.disconnect();
      mo.disconnect();
      term.textarea?.removeEventListener("paste", onPaste, true);
      for (const name of imeEvents) term.textarea?.removeEventListener(name, anchorIme);
      term.textarea?.removeEventListener("blur", dropIme);
      keepIme.disconnect();
      el.removeEventListener("mousedown", onRightButton, true);
      el.removeEventListener("mouseup", onRightButton, true);
      if (id) void backend.close(id);
      if (noticeTimer.current) clearTimeout(noticeTimer.current);
      prediction.dispose();
      predictionRef.current = null;
      termRef.current = null;
      term.dispose();
    };
    // spec 和 backend 在一个窗格的生命周期内不变
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 设置、广播、交给 AI 变了：马上重新判断要不要显示预测
  useEffect(() => { predictionRef.current?.schedule(); }, [prediction, broadcasting]);

  useEffect(() => {
    if (visible) resizeRef.current();
  }, [visible]);

  // 设置变化时更新已打开的终端（字号变化后行列会变，需要重新计算尺寸）
  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.cursorStyle = opts.cursorStyle;
    term.options.cursorWidth = opts.cursorWidth;
    term.options.cursorBlink = opts.cursorBlink;
    term.options.scrollback = opts.scrollback;
    if (term.options.fontSize !== opts.fontSize) {
      term.options.fontSize = opts.fontSize;
      resizeRef.current();
    }
  }, [opts.cursorStyle, opts.cursorWidth, opts.cursorBlink, opts.scrollback, opts.fontSize]);

  return (
    <div
      className="term-pane"
      style={{ display: visible ? "flex" : "none" }}
      onContextMenu={(e) => {
        const term = termRef.current;
        if (!term) return;
        // 网页自带的右键菜单一律不要
        e.preventDefault();
        const selection = term.getSelection();
        const program = term.modes.mouseTrackingMode !== "none";
        const fullScreen = term.buffer.active.type === "alternate";
        const quick = cb.current.opts.rightClickCopyPaste;
        const paste = () => { void readClipboardText().then((text) => pasteRef.current(text), (err: unknown) => showNotice.current(formatError(err))).finally(() => term.focus()); };
        const copy = (text: string) => { copyText.current(text); term.clearSelection(); term.focus(); };
        // 有 Pierlink 自己的选中（全屏程序里按住 Shift 拖选的也算）：程序里、或开了“右键复制粘贴”时直接复制
        if (selection && (program || fullScreen || quick)) { copy(selection); return; }
        // 程序开了鼠标模式（Claude Code、vim 的 mouse=a 等）：右键交给程序。不粘贴：程序里的选中 Pierlink 看不到，
        // 当成“没选中”去粘贴，会和程序自己的复制（OSC 52）一起触发
        if (program) return;
        // 全屏程序里不弹菜单；开了“右键复制粘贴”时没选中就粘贴
        if (fullScreen || quick) { if (quick) paste(); return; }
        const dir = cb.current.cwd ?? null;
        const folder = spec.kind === "local" ? explorerFolder(spec.shellId, dir) : null;
        const split = cb.current.onSplit;
        const can = cb.current.canSplit ?? (() => true);
        ctx.open(e, [
          { id: "copy", label: t("terminal.copy"), shortcut: "Ctrl C", disabled: !selection, onSelect: () => copy(selection) },
          { id: "paste", label: t("terminal.paste"), shortcut: "Ctrl V", onSelect: paste },
          { kind: "separator", id: "s1" },
          { id: "cwd", label: t("terminal.copyCwd"), disabled: !dir, onSelect: () => { if (dir) copyText.current(dir); term.focus(); } },
          ...(spec.kind === "local" && inTauri
            ? [{ id: "explorer", label: t("terminal.openInExplorer"), disabled: !folder, onSelect: () => {
                if (folder) void commands.filesOpenDefault(folder).then((r) => { if (r.status === "error") showNotice.current(formatError(r.error)); });
              } }]
            : []),
          ...(split
            ? [
                { kind: "separator" as const, id: "s2" },
                { id: "splitRow", label: t("terminal.splitRow"), disabled: !can("row"), onSelect: () => split("row") },
                { id: "splitCol", label: t("terminal.splitCol"), disabled: !can("col"), onSelect: () => split("col") },
              ]
            : []),
        ]);
      }}
    >
      <div ref={host} className="term-host" />
      {steps && !steps.failed && !ended && <ConnectProgress stage={steps.stage} start={steps.start} />}
      {ended && !endsInline(ended) && (
        // 登录失败、没能打开：盖住终端内容，说明原因并提供按钮
        <div className={`term-ended term-ended--${ended.kind}`} role="alert">
          <div className="term-ended__box">
            {steps?.failed && <ConnectSteps stage={steps.stage} failed />}
            <div className="term-ended__title">
              {ended.kind === "passwordNeeded" ? t("terminal.endedPasswordNeeded")
                : ended.kind === "authFailed" ? t("terminal.endedAuth")
                : t("terminal.endedFailed")}
            </div>
            <div className="term-ended__detail">
              {ended.kind === "passwordNeeded" ? t("terminal.endedPasswordNeededDetail")
                : ended.kind === "authFailed" ? t(ended.tooManyAuth ? "terminal.endedTooManyAuthDetail" : ended.twoFactor ? "terminal.endedTwoFactorDetail" : ended.password ? "terminal.endedAuthPasswordDetail" : "terminal.endedAuthDetail")
                : ended.kind === "failed" ? ended.text : null}
            </div>
            {offerOpenSsh && <div className="term-ended__detail">{t("terminal.openSshOffer")}</div>}
            <div className="term-ended__actions">
              <Button onClick={() => onCloseTab?.()}>{closeLabel ?? t("terminal.closeTab")}</Button>
              {offerOpenSsh && <Button className="term-ended__openssh" onClick={() => void switchToOpenSsh()}>{t("terminal.useOpenSsh")}</Button>}
              <Button variant="primary" autoFocus onClick={() => reopenRef.current(true)}>
                {spec.kind === "local" ? t("terminal.reopen") : t("terminal.reconnect")}
              </Button>
            </div>
            <div className="term-ended__hint">{t("terminal.endedEnterHint")}</div>
          </div>
        </div>
      )}
      {ended && endsInline(ended) && (
        // 断开、退出：内容留在终端里，右下角放“关闭”和“重新连接”（按 Enter 也能重新连接）
        <div className="term-endbar">
          {onCloseTab && <Button size="sm" onClick={() => onCloseTab()}>{closeLabel ?? t("terminal.closeTab")}</Button>}
          <Button size="sm" variant="primary" onClick={() => reopenRef.current(true)}>
            {spec.kind === "local" ? t("terminal.reopen") : t("terminal.reconnect")}
          </Button>
        </div>
      )}
      {fileDrop && <div className="term-filedrop" aria-hidden="true"><span>{t("terminal.fileDrop")}</span></div>}
      {notice && (
        <div className="term-notice" role="status">
          {notice}
        </div>
      )}
      {ctx.node}
    </div>
  );
}
