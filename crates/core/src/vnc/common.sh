# 专用目录及进程身份检查，不读取凭据。
export LC_ALL=C
umask 077
exec 2>/dev/null
state_dir=$HOME/.local/state/pierlink/vnc
binroot=$HOME/.local/share/pierlink/vnc
uid=$(id -u) || exit 1
json() {
    printf '%s' "$1" | od -An -v -tu1 | awk 'BEGIN {printf "\""} {for(i=1;i<=NF;i++){n=$i;if(n==34 || n==92) printf "\\%c",n;else if(n<32) printf "\\u%04x",n;else printf "%c",n}} END {printf "\""}'
}
fail() {
    printf '{"error":"vnc.%s","reason":' "$1"; json "${3:-操作无法完成}"
    printf ',"reason_key":'; json "${2:-vnc.operation_failed}"
    error_params
    if [ "$1" = start_timeout ]; then
        printf ',"log":'; json "${3:-操作无法完成}"
        printf ',"log_key":'; json "${2:-vnc.operation_failed}"
    fi
    if [ "$1" = download_failed ]; then printf ',"source":'; json "${source:-未知}"; if [ -z "${source:-}" ]; then printf ',"source_key":"vnc.unknown_source"'; fi; fi
    if [ "$1" = xkb_unavailable ]; then printf ',"install_command":'; json "$(xkb_install_command)"; fi
    printf '}'; exit 0
}
# 变化的数值与库名独立返回，供原因译文插值。
error_params() {
    if [ -n "${apt_status:-}" ]; then printf ',"exit_code":'; json "$apt_status"; fi
    if [ -n "${libs:-}" ]; then printf ',"libs":'; json "$libs"; fi
}
safe_path() {
    case "$1" in "$HOME"/*) rest=${1#"$HOME"/}; current=$HOME;; *) fail unsafe_path;; esac
    while [ -n "$rest" ]; do
        component=${rest%%/*}
        case "$component" in ''|.|..) fail unsafe_path;; esac
        current=$current/$component
        [ ! -L "$current" ] || fail unsafe_path
        case "$rest" in */*) rest=${rest#*/};; *) rest=;; esac
    done
}
private_dir() {
    safe_path "$1"
    mkdir -p -- "$1" && chmod 700 -- "$1" || fail permission_denied
}
prune_uploads() {
    # 连接中断遗留的独占上传目录最多保留七天。
    for staged in "$binroot"/.upload-*; do
        [ -d "$staged" ] && [ ! -L "$staged" ] || continue
        [ -f "$staged/unpack/.pierlink-created" ] || continue
        safe_path "$staged"
        age=$(($(date +%s)-$(stat -c %Y "$staged")))
        [ "$age" -le 604800 ] || rm -rf -- "$staged"
    done
}
# 在超时后回收本调用启动的进程。
bounded() (
    limit=$1; shift
    # 独立进程组同时回收 apt 等程序派生的下载进程。
    if command -v setsid >/dev/null; then setsid "$@" </dev/null & job=$!; target=-$job
    else "$@" </dev/null & job=$!; target=$job; fi
    ( sleep "$limit" & timer=$!; trap 'kill "$timer" 2>/dev/null; wait "$timer"; exit' TERM HUP INT
      wait "$timer"; /bin/kill -KILL -- "$target" 2>/dev/null ) >/dev/null 2>&1 & watchdog=$!
    trap '/bin/kill -KILL -- "$target" 2>/dev/null; kill "$watchdog" 2>/dev/null; wait; exit 1' HUP INT TERM
    result=0; wait "$job" || result=$?
    kill "$watchdog" 2>/dev/null || :; wait "$watchdog" 2>/dev/null || :
    exit "$result"
)
remove_fallback_dir() {
    case "$1" in "/tmp/pierlink-vnc-$uid/"*) rmdir -- "/tmp/pierlink-vnc-$uid" 2>/dev/null || :;; esac
}
remove_x_files() {
    # 强制结束时 X server 来不及清理；锁必须仍记录本次启动的 PID。
    xlock=/tmp/.X$1-lock
    if [ -f "$xlock" ] && [ ! -L "$xlock" ] && [ "$(cat "$xlock" | tr -d ' \n')" = "$2" ]; then
        rm -f -- "/tmp/.X11-unix/X$1" "$xlock"
    fi
}
identity() {
    [ -r "/proc/$1/stat" ] && [ "$(awk '$1=="Uid:" {print $2}' "/proc/$1/status")" = "$uid" ] || return 1
    # comm 可含空格和括号；最后一个右括号之后才是固定字段。
    sed 's/^.*) //' "/proc/$1/stat" | awk '$1!="Z" {print $20 " " $3}'
}
prune_logs() {
    # 正在运行和正在启动的日志不计入；结束的日志仅保留最近五份。
    count=0
    for name in $(find "$state_dir" -maxdepth 1 -type f -name '*.log' -printf '%T@ %f\n' | sort -rn | awk '{print $2}'); do
        n=${name%.log}; case "$n" in ''|*[!0-9]*) continue;; esac
        [ ! -e "$state_dir/$n.json" ] && [ ! -d "$state_dir/.$n.launch" ] || continue
        count=$((count+1)); [ "$count" -le 5 ] || rm -f -- "$state_dir/$name"
    done
}
sha256_of() { sha256sum | awk '{print $1}'; }

shell_quote() { printf "'"; printf %s "$1" | sed "s/'/'\\\\''/g"; printf "'"; }
# 日志每行最多 4096 字节、每份最多 1 MiB；敏感字段整行屏蔽。
write_log() {
    awk -v file="$1" '
    {
        line=substr($0,1,4096); lower=tolower(line)
        if(lower ~ /-----begin .*private key-----/) private=1
        secret=private || lower ~ /password|passwd|passphrase|api.?key|authorization|secret|token|cookie|密码|口令|私钥|密钥/
        if(lower ~ /-----end .*private key-----/) private=0
        if(secret) line="***"
        else {
            n=split(line,words,/[^a-fA-F0-9]/)
            for(i=1;i<=n;i++) if(length(words[i])==32) {line="***";break}
        }
        if(bytes+length(line)+1>1048576) {close(file);printf "%s","" > file;close(file);bytes=0}
        print line >> file;fflush(file);bytes+=length(line)+1
    }'
}

# 验证完毕后才切换目录，每类组件仅保留当前版本。
install_prefix() {
    safe_path "$binroot/$version"
    # 安装切换串行化；已有版本只有在完整验证后才替换。
    mkdir "$binroot/.install-lock" || fail locked
    install_locked=true
    trap 'rmdir "$binroot/.install-lock" 2>/dev/null; rm -rf -- "$work"' 0
    if [ -d "$binroot/$version" ]; then
        [ -f "$binroot/$version/.pierlink-created" ] || fail unsafe_path
        mv -- "$binroot/$version" "$work/old" || fail permission_denied
    fi
    printf true > "$work/unpack/.pierlink-created"
    mv -- "$work/unpack" "$binroot/$version" || {
        [ ! -d "$work/old" ] || mv -- "$work/old" "$binroot/$version"
        fail permission_denied
    }
    for marker in "$binroot"/*/.pierlink-created; do
        [ -f "$marker" ] || continue
        candidate=${marker%/.pierlink-created}
        [ "$candidate" != "$binroot/$version" ] || continue
        safe_path "$candidate"
        rm -rf -- "$candidate"
    done
}
