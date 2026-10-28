# 状态文件作为字符串交给 Rust 解析；这里只提取有界的数字 PID。
printf '['
comma=
for file in "$state_dir"/*.json; do
    [ -f "$file" ] && [ ! -L "$file" ] || continue
    n=${file##*/}; n=${n%.json}
    case "$n" in ''|*[!0-9]*) continue;; esac
    [ "${#n}" -le 5 ] || continue
    [ "$(wc -c < "$file")" -le 65536 ] || continue
    content=$(cat "$file")
    pid=$(printf %s "$content" | sed -n 's/.*"xvnc_pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
    boot=$(cat /proc/sys/kernel/random/boot_id)
    recorded_boot=$(printf %s "$content" | sed -n 's/.*"boot_id"[[:space:]]*:[[:space:]]*"\([a-zA-Z0-9-]*\)".*/\1/p')
    ident=; comm=
    case "$pid" in ''|*[!0-9]*) :;; *)
        if [ "${#pid}" -le 10 ] && [ "$recorded_boot" = "$boot" ]; then
            ident=$(identity "$pid")
            [ -z "$ident" ] || comm=$(cat "/proc/$pid/comm")
        fi;;
    esac
    printf '%s{"file":%s,"content":' "$comma" "$n"; json "$content"
    printf ',"boot_id":'; json "$boot"
    printf ',"identity":'; json "$ident"
    printf ',"comm":'; json "$comm"; printf '}'
    comma=,
done
printf ']'
