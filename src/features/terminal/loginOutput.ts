/** 终端控制序列和换行以外的控制字符（判断一段输出里有没有看得见的字） */
const VT_CONTROLS = /\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)?|\x1bP[^\x1b]*(?:\x1b\\)?|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[ -/]*[0-~]?|[\x00-\x09\x0b-\x1f\x7f]/g;
/** ssh 登录被拒、连接被关时打印的一行：之后马上就是登录失败或断开的提示 */
const LOGIN_DENIED = /^(?:[^:]*: )?(?:Permission denied|Authentication failed|Too many authentication failures|Received disconnect from|Disconnected from|Connection closed by)\b/i;

/**
 * 连接卡片盖着时来了一段输出，卡片要不要继续盖着。
 * 只有控制序列（看不见），或者全是登录被拒的话（密码不对时 ssh 打印的 Permission denied, please try again.）时继续盖着：
 * 后面紧跟着失败的提示块，让开会先露出一个几乎空白的终端。其他看得见的字（密码提示、警告、欢迎信息）让开。
 * @param chunk 这次的输出
 * @returns true 表示继续盖着
 */
export function keepsConnectCard(chunk: string): boolean {
  return chunk
    .replace(VT_CONTROLS, "")
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .every((line) => LOGIN_DENIED.test(line));
}

/**
 * 写进终端的数据是不是“有人在打字”：普通文字、控制键、Esc、方向键和编辑键、Alt+键、粘贴算；
 * 程序查询后 xterm 自动回的应答（设备属性、光标位置、颜色、焦点进出）和鼠标报告不算
 * （和宿主判断“电脑上有人在打字”的规则一样）。
 * @param data xterm 给出的一段输入
 * @returns true 表示像打字
 */
export function looksTyped(data: string): boolean {
  if (!data.startsWith("\x1b")) return true;
  const rest = data.slice(1);
  if (!rest || rest.startsWith("[200~")) return true;
  const head = rest[0];
  if (head === "[") {
    // CSI：方向键、Home/End、Shift+Tab（可带 1;5 这类修饰键参数），编辑键和 F5 以上（数字加 ~），
    // kitty 键盘协议的按键（数字加 u；应答带 ? 不算）
    const body = rest.slice(1);
    if (!body) return false;
    const last = body[body.length - 1];
    const params = body.slice(0, -1);
    const numeric = /^[0-9;]*$/.test(params);
    if ("ABCDHFZ".includes(last)) return numeric;
    if (last === "~" || last === "u") return numeric && params.length > 0;
    // 带修饰键的 F1、F2、F4（Shift+F1 是 1;2P）；F3 的 1;NR 和光标位置应答长得一样，按应答算
    if ("PQS".includes(last)) return /^1;\d+$/.test(params);
    return false;
  }
  // SS3：应用模式的方向键、Home/End、F1–F4，小键盘应用模式的数字和符号（0–9 是 p–y，Enter 是 M）
  if (head === "O") return rest.length === 2 && "ABCDHFPQRSjklmnopqrstuvwxyMX".includes(rest[1]);
  // OSC、DCS 等应答
  if ("]P_^X".includes(head)) return false;
  // Alt+键：ESC 后面跟一个字符
  return [...rest].length === 1;
}
