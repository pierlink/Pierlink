# 仅使用 POSIX sh；标准错误不含服务器原文。
set -eu
exec 2>/dev/null
umask 077
export LC_ALL=C
fail() { printf '{"error":"deploy.%s"}' "$1"; exit 0; }
json() {
    printf '%s' "$1" | od -An -v -tu1 | awk 'BEGIN { printf "\"" } { for(i=1;i<=NF;i++) { n=$i; if(n==34 || n==92) printf "\\%c",n; else if(n<32) printf "\\u%04x",n; else printf "%c",n } } END { printf "\"" }'
}
if ! command -v sha256sum >/dev/null && ! command -v shasum >/dev/null && ! command -v openssl >/dev/null; then fail no_sha256_tool; fi
sha256_of() {
    if command -v sha256sum >/dev/null; then sha256sum | awk '{print $1}'
    elif command -v shasum >/dev/null; then shasum -a 256 | awk '{print $1}'
    elif command -v openssl >/dev/null; then openssl dgst -sha256 | awk '{print $NF}'
    else fail no_sha256_tool; fi
}
# 拒绝经符号链接写出托管目录；HOME 本身允许为实际登录目录。
safe_path() {
    case "$1" in "$HOME"/*) ;; *) fail unsafe_path;; esac
    rest=${1#"$HOME"/}; current=$HOME
    while [ -n "$rest" ]; do
        component=${rest%%/*}
        case "$component" in ''|.|..) fail unsafe_path;; esac
        current=$current/$component
        [ ! -L "$current" ] || fail unsafe_path
        case "$rest" in */*) rest=${rest#*/};; *) rest=;; esac
    done
}
state=$HOME/.local/state/pierlink
binroot=$HOME/.local/share/pierlink/bin
runtime=${XDG_RUNTIME_DIR:-}
if [ -n "$runtime" ] && [ ! -d "$runtime" ]; then runtime=; fi
if [ -z "$runtime" ] && [ -d "/run/user/$(id -u)" ]; then runtime=/run/user/$(id -u); fi
if [ -n "$runtime" ]; then export XDG_RUNTIME_DIR=$runtime; fi
systemd=none
if command -v systemctl >/dev/null; then
    if systemctl --user show-environment >/dev/null 2>&1; then systemd=available
    elif [ -S "$runtime/bus" ]; then systemd=bus_unreachable
    else systemd=user_manager_not_running; fi
fi
linger=null
if command -v loginctl >/dev/null; then
    case "$(loginctl show-user "$(id -un)" -p Linger 2>/dev/null || :)" in Linger=yes) linger=true;; Linger=no) linger=false;; esac
fi
kill_users=null
for conf in /etc/systemd/logind.conf /etc/systemd/logind.conf.d/*.conf; do
    [ -f "$conf" ] || continue
    setting=$(sed -n 's/^[[:space:]]*KillUserProcesses[[:space:]]*=[[:space:]]*\(yes\|no\)[[:space:]]*$/\1/p' "$conf" | tail -n 1)
    case "$setting" in yes) kill_users=true;; no) kill_users=false;; esac
done
lock=$state/deploy.lock
locked=false
taken_over=false
temporary=
reaping=false
cleanup() {
    if [ "$reaping" = true ]; then rmdir "$state/deploy.reap" || :; fi
    if [ -n "$temporary" ]; then rm -f -- "$temporary"; fi
    if [ "$locked" = true ] && [ "$(cat "$lock/owner" 2>/dev/null || :)" = "$owner" ]; then
        rm -f -- "$lock/owner"; rmdir -- "$lock" || :
    fi
}
trap cleanup 0
trap 'exit 1' HUP INT TERM
acquire() {
    safe_path "$lock"
    mkdir -p -- "$state" || fail permission_denied
    count=0
    while ! mkdir -- "$lock" 2>/dev/null; do
        if [ -d "$state/deploy.reap" ]; then
            reap_age=$(($(date +%s)-$(stat -c %Y "$state/deploy.reap")))
            if [ "$reap_age" -gt 600 ]; then rmdir "$state/deploy.reap" || :; fi
        fi
        now=$(date +%s); modified=$(stat -c %Y "$lock" 2>/dev/null || printf '%s' "$now")
        if [ "$((now-modified))" -gt 600 ] && mkdir "$state/deploy.reap" 2>/dev/null; then
            reaping=true
            modified=$(stat -c %Y "$lock" 2>/dev/null || printf '%s' "$now")
            if [ "$((now-modified))" -gt 600 ]; then
                rm -f -- "$lock/owner"; rmdir -- "$lock" || :
                taken_over=true
            fi
            rmdir "$state/deploy.reap"
            reaping=false
        fi
        [ "$count" -lt 30 ] || fail locked
        count=$((count+1)); sleep 2
    done
    locked=true
    printf '%s' "$owner" > "$lock/owner"
}
assert_stopped() {
    for proc in /proc/[0-9]*/cmdline; do
        if tr '\000' '\n' < "$proc" 2>/dev/null | grep -Fx -- "--home=$dir/home" >/dev/null; then fail instance_running; fi
    done
}
ports() {
    [ -r /proc/net/tcp ] || fail cannot_probe_ports
    cat /proc/net/tcp
    if [ -r /proc/net/tcp6 ]; then cat /proc/net/tcp6; fi
}

manifest_snapshot() {
    for manifest in "$state"/*/manifest.json; do
        [ -f "$manifest" ] || continue
        [ "$manifest" != "$own_manifest" ] || continue
        printf '%s\000%s\000' "$manifest" "$(sha256_of < "$manifest")"
    done | sha256_of
}
