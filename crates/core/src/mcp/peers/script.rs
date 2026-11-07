//! 服务器上运行的脚本（经 run_script 从标准输入发送）：写、删伙伴文件，读伙伴文件并找出 AI 所在终端的名牌。
use super::{PinRecord, plain};

/// 写这台电脑的伙伴文件 `~/.pierlink/peers/<实例号>.json`（目录 0700、文件 0600，先写临时文件再改名）。
/// 成功输出 {"ok":true}；令牌只在标准输入的正文里，不进进程参数。
///
/// :param instance: 实例号（只含字母数字和 -_，不合规时脚本直接失败）
/// :param computer: 电脑名
/// :param port: 这台电脑的入口在服务器上监听的端口
/// :param token: 入口令牌
/// :param written_at: 写的时间（Unix 毫秒）
/// :param pin: 这台电脑上的用户做的指定
/// :return: 脚本正文
pub fn peer_file_script(
    instance: &str,
    computer: &str,
    port: u16,
    token: &str,
    written_at: u64,
    pin: Option<&PinRecord>,
) -> String {
    if !plain(instance, 64) {
        return "printf '%s\\n' '实例号不合规' >&2; exit 1\n".into();
    }
    let value = serde_json::json!({
        "instance": instance,
        "computer": computer,
        "port": port,
        "token": token,
        "written_at": written_at,
        "pin": pin,
    });
    format!(
        r#"set -eu
umask 077
dir="$HOME/.pierlink"
peers="$dir/peers"
part=''
fail() {{ printf '%s\n' '无法安全写入伙伴文件' >&2; exit 1; }}
trap 'if [ -n "$part" ]; then rm -f -- "$part"; fi' 0
trap 'exit 1' HUP INT TERM
[ ! -L "$dir" ] || fail
mkdir -p -- "$dir" || fail
chmod 700 -- "$dir" || fail
[ ! -L "$peers" ] || fail
mkdir -p -- "$peers" || fail
chmod 700 -- "$peers" || fail
[ ! -d "$peers/{instance}.json" ] || fail
part=$(mktemp "$peers/.peer.XXXXXX") || fail
cat > "$part" <<'PIERLINK_PEER_JSON'
{value}
PIERLINK_PEER_JSON
chmod 600 -- "$part" || fail
mv -f -- "$part" "$peers/{instance}.json" || fail
part=''
printf '%s\n' '{{"ok":true}}'
"#
    )
}

/// 删这台电脑的伙伴文件（没有也算成功），输出 {"ok":true}
///
/// :param instance: 实例号
/// :return: 脚本正文
pub fn remove_peer_file_script(instance: &str) -> String {
    if !plain(instance, 64) {
        return "printf '%s\\n' '实例号不合规' >&2; exit 1\n".into();
    }
    format!(
        r#"set -eu
dir="$HOME/.pierlink"
if [ -L "$dir" ] || [ -L "$dir/peers" ]; then printf '%s\n' '伙伴目录不能是符号链接' >&2; exit 1; fi
rm -f -- "$dir/peers/{instance}.json"
printf '%s\n' '{{"ok":true}}'
"#
    )
}

/// 读伙伴文件；给了入口端口时还找出正连着这个端口的 AI 所在终端的名牌。只读，输出一行 JSON：
/// `{"peers":["文件内容",…],"markers":["名牌",…]}`（文件内容去掉控制字符和非 UTF-8 字节后作为字符串；不输出环境原文）。
/// 只读两小时内写过的、文件名只含字母数字和 -_. 的前 16 个（连着的电脑每半小时重写一次；关掉的电脑会删自己的，
/// 没删掉的过两小时也不算了）。
///
/// 找名牌：/proc/net/tcp 里本用户、对端端口是入口端口的已建立连接 → 按 socket inode 找到进程（AI 客户端）→
/// 它在 screen 里（有 STY）就取连着这个会话的 screen 客户端的名牌；在 tmux 里（有 TMUX）就取连着这个会话的
/// tmux 客户端的名牌；都不是就取它自己的名牌。
///
/// :param port: 入口在服务器上监听的端口；None 只读伙伴文件
/// :return: 脚本正文
pub fn discover_script(port: Option<u16>) -> String {
    let attach = port.map(attach_script).unwrap_or_default();
    format!(
        r#"export LC_ALL=C
exec 2>/dev/null
dir="$HOME/.pierlink/peers"
printf '{{"peers":['
sep=''
if [ -d "$dir" ] && [ ! -L "$dir" ] && [ ! -L "$HOME/.pierlink" ]; then
  set -f
  n=0
  for f in $(find "$dir" -maxdepth 1 -type f -name '*.json' -mmin -120 2>/dev/null); do
    case "${{f##*/}}" in *[!A-Za-z0-9_.-]*) continue;; esac
    [ -f "$f" ] && [ ! -L "$f" ] || continue
    n=$((n + 1)); [ "$n" -le 16 ] || break
    c=$(head -c 8192 -- "$f" | {{ iconv -c -f UTF-8 -t UTF-8 2>/dev/null || cat; }} | tr -d '\000-\037' | sed 's/\\/\\\\/g; s/"/\\"/g')
    printf '%s"%s"' "$sep" "$c"
    sep=','
  done
  set +f
fi
printf '],"markers":['
{attach}printf ']}}\n'
"#
    )
}

/// 找名牌的部分：输出逗号分隔的名牌 JSON 字符串（名牌只含字母数字和 -_，不用转义）
fn attach_script(port: u16) -> String {
    format!(
        r#"port={port}
uid=$(id -u)
h=$(printf ':%04X' "$port")
inodes=$(for t in /proc/net/tcp /proc/net/tcp6; do [ -r "$t" ] && awk -v h="$h" -v u="$uid" 'FNR > 1 && $4 == "01" && $8 == u && substr($3, length($3) - 4) == h {{ print $10 }}' "$t"; done)
pids=''
if [ -n "$inodes" ]; then
  want=$(printf '%s ' $inodes)
  pids=$(ls -l /proc/[0-9]*/fd 2>/dev/null | awk -v want="$want" '
    BEGIN {{ n = split(want, a, " "); for (i = 1; i <= n; i++) if (a[i] != "" && a[i] != "0") w["socket:[" a[i] "]"] = 1 }}
    /^\/proc\/[0-9]+\/fd:$/ {{ split($0, d, "/"); pid = d[3]; next }}
    pid != "" && ($NF in w) {{ print pid }}' | sort -u)
fi
# 环境变量、命令行按 NUL 分隔：先换成换行再读（不依赖 awk 支不支持 RS="\0"）
val() {{ tr '\000' '\n' < "/proc/$1/environ" | sed -n "s/^$2=//p" | head -n 1; }}
markers=' '
add() {{
  case "$1" in ''|*[!A-Za-z0-9_-]*) return 0;; esac
  [ ${{#1}} -le 64 ] || return 0
  case "$markers" in *" $1 "*) return 0;; esac
  markers="$markers$1 "
}}
# screen：会话说明（-r/-x/-R/-d/-D 后面的、或 -S 的）等于会话名、进程号或“进程号.会话名”（或是会话名的开头）的客户端；
# 没有这样的客户端时，取没写会话说明的客户端
screen_clients() {{
  spid=${{1%%.*}}; sname=${{1#*.}}
  hits=''; loose=''
  for p in /proc/[0-9]*; do
    [ -r "$p/environ" ] || continue
    IFS= read -r comm < "$p/comm" || continue
    [ "$comm" = screen ] || continue
    spec=$(tr '\000' '\n' < "$p/cmdline" | awk '
      NR == 1 {{ if ($0 ~ /SCREEN$/) {{ bad = 1; exit }} next }}
      skip != "" {{ if (skip == "S" && spec == "") spec = $0; skip = ""; next }}
      $0 == "-S" {{ skip = "S"; next }}
      $0 ~ /^-(c|e|h|p|t|T|s|Logfile)$/ {{ skip = "x"; next }}
      $0 ~ /^-(ls|list|wipe|v|version|help)$/ || $0 ~ /^-[A-Za-z]*[XQ]/ {{ bad = 1; exit }}
      /^-/ {{ next }}
      spec == "" {{ spec = $0 }}
      END {{ if (bad) print "-"; else print "=" spec }}')
    case "$spec" in
      -*) continue;;
      =) loose="$loose ${{p##*/}}";;
      *) s=${{spec#=}}
        case "$s" in
          "$sname"|"$spid"|"$spid.$sname") hits="$hits ${{p##*/}}";;
          *) case "$sname" in "$s"*) hits="$hits ${{p##*/}}";; esac;;
        esac;;
    esac
  done
  [ -n "$hits" ] || hits=$loose
  for c in $hits; do add "$(val "$c" LC_PIERLINK_TERM)"; done
}}
# tmux：TMUX=套接字,服务进程号,会话号；连着这个会话的客户端进程号
tmux_clients() {{
  sock=${{1%%,*}}; rest=${{1#*,}}; sid=${{rest#*,}}
  case "$sid" in ''|*[!0-9]*) return 0;; esac
  command -v tmux >/dev/null 2>&1 || return 0
  tmux -S "$sock" list-clients -F '#{{client_pid}} #{{session_id}}' 2>/dev/null | while read -r cpid csid; do
    case "$cpid" in ''|*[!0-9]*) continue;; esac
    [ "$csid" = "\$$sid" ] && printf '%s\n' "$cpid"
  done
}}
for pid in $pids; do
  sty=$(val "$pid" STY)
  tm=$(val "$pid" TMUX)
  if [ -n "$sty" ]; then
    screen_clients "$sty"
  elif [ -n "$tm" ]; then
    for c in $(tmux_clients "$tm"); do add "$(val "$c" LC_PIERLINK_TERM)"; done
  else
    add "$(val "$pid" LC_PIERLINK_TERM)"
  fi
done
sep=''
for m in $markers; do printf '%s"%s"' "$sep" "$m"; sep=','; done
"#
    )
}
