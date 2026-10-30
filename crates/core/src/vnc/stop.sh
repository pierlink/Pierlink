safe_path "$state_dir/$display.json"
[ "$xauth_file" = "$state_dir/$display.xauth" ] && [ "$log" = "$state_dir/$display.log" ] || fail stale
case "$socket" in "$state_dir/$display.sock") safe_path "$socket";;
    "/tmp/pierlink-vnc-$uid/$display.sock")
        fallback=/tmp/pierlink-vnc-$uid
        [ ! -L "$fallback" ] || fail stale
        # 重启后临时目录可能已被系统删除，仍应允许清理旧状态。
        if [ -e "$fallback" ]; then
            [ -d "$fallback" ] && [ "$(stat -c %u "$fallback")" = "$uid" ] || fail stale
        fi;;
    *) fail stale;;
esac
lock=$state_dir/.$display.launch
mkdir "$lock" || fail stale
trap 'rmdir -- "$lock" 2>/dev/null' 0
[ "$(cat "$state_dir/$display.json")" = "$expected" ] || fail stale
current_boot=$(cat /proc/sys/kernel/random/boot_id)
alive=false
if [ "$current_boot" = "$boot" ] && [ "$(identity "$pid")" = "$starttime $pgid" ]; then
    case "$(cat "/proc/$pid/comm")" in Xvnc|Xtigervnc) alive=true;; esac
fi
if [ "$alive" = true ] && [ "$stop" = false ]; then printf '{"removed":false}'; exit; fi
if [ "$alive" = true ]; then
    # 监督进程就是进程组首领；保留其启动标识，防止等待期间 PID 被复用。
    leader=$(identity "$pgid")
    [ -n "$leader" ] && [ "${leader#* }" = "$pgid" ] || fail stale
    # dash 的内建 kill 不接受 --，使用系统程序传递负进程组。
    /bin/kill -TERM -- "-$pgid" || fail stale
    sleep 5
    if [ "$(identity "$pgid")" = "$leader" ] || [ "$(identity "$pid")" = "$starttime $pgid" ]; then
        /bin/kill -KILL -- "-$pgid" 2>/dev/null || :
    fi
    # 监督进程也可能被强制结束；等自己的 X server 消失后再兜底清理。
    i=0
    while [ "$(identity "$pid")" = "$starttime $pgid" ] && [ "$i" -lt 10 ]; do sleep .1; i=$((i+1)); done
    [ "$(identity "$pid")" != "$starttime $pgid" ] || fail stale
    safe_path "$xauth_file"
    rm -f -- "$xauth_file"
    remove_x_files "$display" "$pid"
fi
# 旧机器启动记录只删除状态；清理模式可移除没有内核监听者的残留路径。
if [ "$current_boot" = "$boot" ] || [ "$stop" = false ]; then
    if ! awk -v path="$socket" 'index($0,path) && substr($0,length($0)-length(path)+1)==path {found=1} END {exit !found}' /proc/net/unix; then
        [ -L "$socket" ] || rm -f -- "$socket"
        remove_fallback_dir "$socket"
    fi
fi
safe_path "$state_dir/$display.cache"
rm -rf -- "$state_dir/$display.cache"
safe_path "$state_dir/$display.xkb"
xkb_namespace_remove "$state_dir/$display.xkb"
rm -f -- "$state_dir/$display.json"
rmdir -- "$lock"; trap - 0
prune_logs
printf '{"removed":true}'
