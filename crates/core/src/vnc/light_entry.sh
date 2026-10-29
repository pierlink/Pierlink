light_prefix=
for marker in "$HOME"/.local/share/pierlink/vnc-light/*/.pierlink-created; do
    [ -f "$marker" ] || continue
    candidate=${marker%/.pierlink-created}
    safe_path "$candidate"
    if [ -x "$candidate/session" ]; then light_prefix=$candidate; break; fi
done
[ -n "$light_prefix" ] || fail light_unavailable '轻量桌面尚未获取完成'
prefix_environment "$light_prefix"
desktop_name='Openbox'
desktop_names=OPENBOX
desktop_exec=$(shell_quote "$light_prefix/session")
export light_prefix
