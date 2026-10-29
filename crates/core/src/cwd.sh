# 仅输出目录与忙碌状态 JSON，不输出进程环境或其他服务器原文。
exec 2>/dev/null
export LC_ALL=C
if [ ! -d /proc/self ]; then printf null; exit 0; fi
uid=$(id -u) || exit 1
best=0
best_pid=0
for process in /proc/[0-9]*; do
    [ -r "$process/environ" ] || continue
    owner=$(awk '/^Uid:/ {print $2}' "$process/status") || continue
    [ "$owner" = "$uid" ] || continue
    IFS= read -r comm < "$process/comm" || continue
    case "$comm" in bash|zsh|fish|sh|dash|ksh|ksh93|ash|tcsh|csh) ;; *) continue;; esac
    awk -v wanted="$marker" 'BEGIN {RS="\0"} $0==wanted {found=1} END {exit !found}' "$process/environ" || continue
    # 有控制终端的脚本进程仍可能非交互；显式 -i，或没有 -c/脚本参数时才作为交互 shell。
    awk 'BEGIN {RS="\0"} NR==1 {next} skip {skip=0; next}
        !ended && $0=="--" {ended=1; next}
        !ended && ($0=="--rcfile" || $0=="--init-file" || $0=="-C" || $0=="--init-command") {skip=1; next}
        !ended && /^-/ {if ($0=="--interactive" || $0 ~ /^-[^-]*i/) interactive=1; if ($0=="--command" || $0 ~ /^-[^-]*c/) command=1; next}
        {script=1} END {exit !(interactive || (!command && !script))}' "$process/cmdline" || continue
    # stat 的 comm 允许括号和空格，先去掉最后一个右括号及其之前的字段。
    stats=$(cat "$process/stat") || continue
    stats=${stats##*) }
    info=$(printf '%s\n' "$stats" | awk '$5 != 0 {print $20}')
    case "$info" in ''|*[!0-9]*) continue;; esac
    pid=${process##*/}
    if [ "$info" -gt "$best" ] || { [ "$info" -eq "$best" ] && [ "$pid" -gt "$best_pid" ]; }; then
        [ -d "$process/cwd" ] || continue
        best=$info
        best_pid=$pid
    fi
done
if [ "$best_pid" -eq 0 ]; then printf null; exit 0; fi
# 用哨兵保留路径末尾的换行；readlink 自身输出的一个换行另行去掉。
value=$(readlink "/proc/$best_pid/cwd" && printf '.') || { printf null; exit 0; }
value=${value%.}
value=${value%'
'}
stats=$(cat "/proc/$best_pid/stat") || { printf null; exit 0; }
stats=${stats##*) }
busy=$(printf '%s\n' "$stats" | awk -v start="$best" 'NF>=20 && $20==start && $5!=0 {if ($6>0 && $6!=$3) print "true"; else print "false"}')
case "$busy" in true|false) ;; *) printf null; exit 0;; esac
printf '{"cwd":'
printf '%s' "$value" | od -An -v -tu1 | awk 'BEGIN {printf "\""} {for(i=1;i<=NF;i++) {n=$i; if(n==34 || n==92) printf "\\%c",n; else if(n<32) printf "\\u%04x",n; else printf "%c",n}} END {printf "\""}'
printf ',"busy":%s}' "$busy"
