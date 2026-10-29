# 独立会话持有 Xvnc 与桌面进程；注销、失败、停止均回收资源。
state_dir=$1; display=$2; binary=$3; socket=$4; geometry=$5
xauth_file=$state_dir/$display.xauth
log=$state_dir/$display.log
lock=$state_dir/.$display.launch
xpid=; desktop_pid=
finish() {
    trap '' TERM HUP INT
    # 进程组仍由本监督进程持有；先温和结束整个桌面，再回收 X server。
    /bin/kill -TERM -- "-$$" 2>/dev/null || :
    if [ -n "$xpid" ]; then
        kill -TERM "$xpid" 2>/dev/null
        i=0
        while [ -n "$(identity "$xpid")" ] && [ "$i" -lt 50 ]; do sleep .1; i=$((i+1)); done
        kill -KILL "$xpid" 2>/dev/null; wait "$xpid" 2>/dev/null
        remove_x_files "$display" "$xpid"
    fi
    rm -f -- "$socket" "$state_dir/$display.json"
    rm -rf -- "$state_dir/$display.cache"
    if [ "${xkb_namespace:-false}" = true ]; then xkb_namespace_remove "$state_dir/$display.xkb"; fi
    remove_fallback_dir "$socket"
    rm -f -- "$xauth_file"
    prune_logs
    # 注销时也清理忽略 TERM 的后代；自身最后退出，进程组号不会先被复用。
    /bin/kill -KILL -- "-$$" 2>/dev/null || :
}
trap finish 0
trap 'exit 0' TERM HUP INT
# 通过有界日志管道持续脱敏，避免桌面程序无限写盘。
mkfifo -m 600 "$lock/log.pipe" || exit 1
(trap '' TERM HUP INT; write_log "$log" < "$lock/log.pipe") &
exec > "$lock/log.pipe" 2>&1
rm -f -- "$lock/log.pipe"
# 仅展开由 keyboard_environment 转义的固定参数，不接受前端命令。
eval "set -- ${xkb_args:-}"
set -- "$binary" ":$display" -rfbport -1 -rfbunixpath "$socket" -rfbunixmode 0600 -SecurityTypes None -auth "$xauth_file" -geometry "$geometry" -depth 24 -desktop "$desktop_name" -nolisten tcp -AlwaysShared -FrameRate 30 "$@"
(
    if [ "${xkb_namespace:-false}" = true ]; then
        xkb_namespace_exec "$state_dir/$display.xkb" start "$@"
    else exec "$@"; fi
) &
xpid=$!
startup_failed() {
    if [ "${xkb_namespace:-false}" = true ] && [ "$(cat "$state_dir/$display.xkb/stage")" != ready ]; then
        xkb_namespace_reason "$state_dir/$display.xkb" > "$lock/xkb-reason"
        printf xkb > "$lock/result"
    else printf failed > "$lock/result"; fi
}
i=0
while [ ! -S "$socket" ]; do
    [ -d "$lock" ] || exit
    [ -n "$(identity "$xpid")" ] || { startup_failed; exit; }
    [ "$i" -lt 100 ] || { startup_failed; exit; }
    sleep .1; i=$((i+1))
done
chmod 600 -- "$socket" || { printf socket > "$lock/result"; exit; }
case "$(cat "/proc/$xpid/comm")" in Xvnc|Xtigervnc) :;; *) startup_failed; exit;; esac
if [ "${xkb_namespace:-false}" = true ]; then
    xlock=/tmp/.X$display-lock
    xsocket=/tmp/.X11-unix/X$display
    # 只收紧本次 Xvnc 创建的文件，命名空间外的属主必须仍是登录用户。
    if [ -e "$xlock" ] && {
        [ -L "$xlock" ] || [ -L "$xsocket" ] ||
        [ "$(cat "$xlock" | tr -d ' \n')" != "$xpid" ] ||
        [ "$(stat -c %u "$xlock")" != "$uid" ] ||
        [ "$(stat -c %u "$xsocket")" != "$uid" ] ||
        ! chmod 600 -- "$xlock" "$xsocket"
    }; then
        printf '无法确认命名空间内 X11 文件的属主和私有权限' > "$lock/xkb-reason"
        printf xkb > "$lock/result"; exit
    fi
fi
# 只核对刚启动进程持有的套接字，禁止任何 TCP 监听。
inodes=$(
    for fd in /proc/"$xpid"/fd/*; do
        link=$(readlink "$fd")
        case "$link" in socket:\[*\]) n=${link#socket:[}; printf '%s\n' "${n%]}";; esac
    done
)
[ -r /proc/net/tcp ] && [ -n "$inodes" ] || { printf tcp > "$lock/result"; exit; }
set -- /proc/net/tcp
if [ -e /proc/net/tcp6 ]; then
    [ -r /proc/net/tcp6 ] || { printf tcp > "$lock/result"; exit; }
    set -- "$@" /proc/net/tcp6
fi
listens=$(awk -v ids="$inodes" 'BEGIN {split(ids,a);for(i in a) wanted[a[i]]=1} $10 in wanted && $4=="0A" {print $10}' "$@") || { printf tcp > "$lock/result"; exit; }
[ -z "$listens" ] || { printf tcp > "$lock/result"; exit; }
set -- $(identity "$xpid")
[ "$#" = 2 ] || { printf failed > "$lock/result"; exit; }
starttime=$1; pgid=$2
[ "$pgid" = "$$" ] || { printf failed > "$lock/result"; exit; }
boot=$(cat /proc/sys/kernel/random/boot_id)
created=$(date +%s)000
{
    printf '{"display":%s,"socket":' "$display"; json "$socket"
    printf ',"xauth":'; json "$xauth_file"; printf ',"log":'; json "$log"
    printf ',"xvnc_pid":%s,"pgid":%s,"starttime":' "$xpid" "$pgid"; json "$starttime"
    printf ',"boot_id":'; json "$boot"; printf ',"desktop_id":'; json "$desktop_id"
    printf ',"desktop_name":'; json "$desktop_name"
    printf ',"created_at":%s,"geometry":' "$created"; json "$geometry"
    printf ',"linger_failed":%s}' "$linger_failed"
} > "$lock/state" || { printf failed > "$lock/result"; exit; }
chmod 600 "$lock/state" && mv -- "$lock/state" "$state_dir/$display.json" || { printf failed > "$lock/result"; exit; }
export DISPLAY=:$display XAUTHORITY=$xauth_file XDG_SESSION_TYPE=x11 XDG_CURRENT_DESKTOP=$desktop_names DESKTOP_SESSION=$desktop_id
unset SESSION_MANAGER DBUS_SESSION_BUS_ADDRESS
if [ -n "${light_prefix:-}" ]; then
    XDG_CACHE_HOME=$state_dir/$display.cache
    private_dir "$XDG_CACHE_HOME"
    export XDG_CACHE_HOME
    light_xresources
    cat > "$XDG_CACHE_HOME/session.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig><type>session</type><listen>unix:abstract=pierlink-vnc-$uid-$display-$$</listen><auth>EXTERNAL</auth><policy context="default"><allow send_destination="*" eavesdrop="true"/><allow eavesdrop="true"/><allow own="*"/></policy></busconfig>
EOF
fi
# 登录 shell 可能重写环境，因此在登录初始化之后再次设置。
session_command="$(session_environment)export DISPLAY=$(shell_quote "$DISPLAY") XAUTHORITY=$(shell_quote "$XAUTHORITY") XDG_SESSION_TYPE=x11 XDG_CURRENT_DESKTOP=$(shell_quote "$desktop_names") DESKTOP_SESSION=$(shell_quote "$desktop_id"); unset SESSION_MANAGER DBUS_SESSION_BUS_ADDRESS; exec $dbus $desktop_exec"
"${SHELL:-/bin/sh}" -l -c "$session_command" & desktop_pid=$!
printf ready > "$lock/result"
wait "$desktop_pid"
