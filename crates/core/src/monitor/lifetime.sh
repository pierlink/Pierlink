{
# 整个复合命令解析完后才执行 read，采样正文不会被误当成控制输入。
# 保持输入端打开作为会话存活信号；断线或主动停止的 EOF 都会回收采样树。
exec 3<&0
plmon_parent=$$
(
    {
        while IFS= read -r plmon_control <&3; do
            printf '%s\n' "$plmon_control" || break
        done
        kill -TERM "$plmon_parent" 2>/dev/null || :
    } | /bin/sh -c PLMON_SCRIPT_QUOTED
) &
plmon_task=$!
plmon_kill_tree() {
    kill -STOP "$1" 2>/dev/null || return
    plmon_children=
    if [ -r "/proc/$1/task/$1/children" ]; then
        read -r plmon_children < "/proc/$1/task/$1/children" || :
    fi
    for plmon_child in $plmon_children; do plmon_kill_tree "$plmon_child"; done
    kill -KILL "$1" 2>/dev/null || :
}
plmon_cleanup() {
    plmon_kill_tree "$plmon_task"
    wait "$plmon_task" 2>/dev/null || :
}
trap 'plmon_cleanup; exit' HUP INT TERM
wait "$plmon_task" 2>/dev/null || :
plmon_cleanup
}
