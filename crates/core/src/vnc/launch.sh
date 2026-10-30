private_dir "$state_dir"
binary_environment "$binary"
keyboard_environment
owned_lock=
handed_off=false
owned_xkb=false
cleanup_launch() {
    if [ -n "$owned_lock" ]; then
        if [ "$handed_off" = false ]; then
            rm -f -- "$state_dir/$display.xauth"
            remove_fallback_dir "${socket:-}"
            if [ "$owned_xkb" = true ]; then xkb_namespace_remove "$state_dir/$display.xkb"; fi
        fi
        rm -f -- "$owned_lock/result" "$owned_lock/state" "$owned_lock/xkb-reason" "$owned_lock/xkb-reason-key"
        rmdir -- "$owned_lock" 2>/dev/null || :
    fi
}
trap cleanup_launch 0
trap 'exit 1' HUP INT TERM
xsessions=${PIERLINK_VNC_XSESSIONS_DIR:-/usr/share/xsessions}
# 仅测试覆盖显示号；生产始终从 20 起。
first=20
if [ -n "${PIERLINK_VNC_XSESSIONS_DIR:-}" ]; then first=90; fi
linger_failed=false
bounded 3 loginctl enable-linger "$uid" >/dev/null 2>&1 || linger_failed=true
command -v setsid >/dev/null && command -v xauth >/dev/null || fail missing_tool vnc.setsid_or_xauth_missing '缺少 setsid 或 xauth'
if command -v dbus-run-session >/dev/null; then
    dbus=$(shell_quote "$(command -v dbus-run-session)")
    if [ -n "${light_prefix:-}" ]; then dbus="$dbus --dbus-daemon=$(shell_quote "$light_prefix/dbus-daemon")"; fi
    dbus="$dbus --"
elif command -v dbus-launch >/dev/null; then dbus="$(shell_quote "$(command -v dbus-launch)") --exit-with-session"
else fail missing_tool vnc.dbus_launcher_missing '缺少 dbus-run-session 或 dbus-launch'; fi
export desktop_id desktop_name desktop_names desktop_exec linger_failed dbus
last=$((first+9))
display=$first
while [ "$display" -le "$last" ]; do
    lock=$state_dir/.$display.launch
    if [ -e "/tmp/.X11-unix/X$display" ] || [ -e "/tmp/.X$display-lock" ] || [ -e "$state_dir/$display.json" ] || ! mkdir -- "$lock"; then
        display=$((display+1)); continue
    fi
    owned_lock=$lock
    handed_off=false
    owned_xkb=false
    safe_path "$state_dir/$display.log"
    safe_path "$state_dir/$display.xauth"
    safe_path "$state_dir/$display.json"
    socket=$state_dir/$display.sock
    attempt=0
    while [ "$attempt" -lt 2 ]; do
        owned_xkb=false
        if [ "${xkb_namespace:-false}" = true ]; then
            safe_path "$state_dir/$display.xkb"
            xkb_namespace_prepare "$state_dir/$display.xkb" || fail xkb_unavailable "$xkb_reason_key" "$xkb_reason"
            owned_xkb=true
        fi
        if [ "${#socket}" -ge 100 ] || [ "$attempt" = 1 ]; then
            fallback=/tmp/pierlink-vnc-$uid
            [ ! -L "$fallback" ] || fail socket_path
            if [ ! -d "$fallback" ]; then mkdir -m 700 -- "$fallback" || fail socket_path; fi
            [ "$(stat -c %u "$fallback")" = "$uid" ] && [ "$(stat -c %a "$fallback")" = 700 ] || fail socket_path
            socket=$fallback/$display.sock
        fi
        [ "${#socket}" -lt 100 ] && [ ! -e "$socket" ] && [ ! -L "$socket" ] || fail socket_path
        cookie=$(mcookie 2>/dev/null || od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
        case "$cookie" in *[!a-fA-F0-9]*|'') fail start_timeout;; esac
        [ "${#cookie}" = 32 ] || fail start_timeout
        xauth -f "$state_dir/$display.xauth" add ":$display" MIT-MAGIC-COOKIE-1 "$cookie" >/dev/null 2>&1 || fail start_timeout
        unset cookie
        chmod 600 "$state_dir/$display.xauth" || fail permission_denied
        # 参数和脚本内容由 Rust 统一引用，Exec 只来自此服务器的 desktop 文件。
        setsid -f sh -c "$supervisor" sh "$state_dir" "$display" "$binary" "$socket" "$geometry" </dev/null >"$state_dir/$display.log" 2>&1 || fail start_timeout
        handed_off=true
        i=0
        while [ ! -f "$lock/result" ] && [ "$i" -lt 130 ]; do sleep .1; i=$((i+1)); done
        result=$(cat "$lock/result")
        if [ "$result" = ready ] && [ -f "$state_dir/$display.json" ]; then
            cat "$state_dir/$display.json"
            rm -f -- "$lock/result"; rmdir -- "$lock"
            prune_logs
            exit 0
        fi
        # 等待监督进程清理结束后再尝试回退路径。
        i=0
        while [ -e "$state_dir/$display.xauth" ] && [ "$i" -lt 60 ]; do sleep .1; i=$((i+1)); done
        [ ! -e "$state_dir/$display.xauth" ] || fail start_timeout 'vnc.desktop_cleanup' '桌面进程尚未完成清理'
        handed_off=false
        rm -f -- "$lock/result"
        if [ "$result" = tcp ]; then rmdir -- "$lock"; fail tcp_listen; fi
        if [ "$result" = xkb ]; then fail xkb_unavailable "$(cat "$lock/xkb-reason-key")" "$(cat "$lock/xkb-reason")"; fi
        if grep -qi 'already active' "$state_dir/$display.log"; then break; fi
        if [ "$attempt" = 0 ] && grep -Eqi 'bind|unix.*socket|rfbunix' "$state_dir/$display.log"; then attempt=1; continue; fi
        rmdir -- "$lock"
        printf '{"error":"vnc.start_timeout","log":'; json "$(tail -n 20 "$state_dir/$display.log")"; printf '}'
        prune_logs
        exit 0
    done
    rmdir -- "$lock"
    owned_lock=
    display=$((display+1))
done
prune_logs
fail display_exhausted
