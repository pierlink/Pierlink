# 系统组件优先，前缀只补充系统缺失的程序、库和数据。
prefix_environment() {
    prefix=$1
    [ -d "$prefix" ] || return 0
    if [ -x "$prefix/usr/bin/xkbcomp" ]; then xkb_prefix_compiler=$prefix/usr/bin/xkbcomp; fi
    case "$prefix" in *:*) fail unsafe_path '安装路径含冒号，无法设置程序和动态库搜索路径';; esac
    PATH=${PATH:-/usr/bin:/bin}:$prefix/usr/bin:$prefix/bin
    XDG_DATA_DIRS=${XDG_DATA_DIRS:-/usr/local/share:/usr/share}:$prefix/usr/share
    XDG_CONFIG_DIRS=${XDG_CONFIG_DIRS:-/etc/xdg}:$prefix/etc/xdg
    library_dirs=
    for library_dir in /lib/*-linux-* /usr/lib/*-linux-* /lib64 /usr/lib64 /lib /usr/lib; do
        [ ! -d "$library_dir" ] || library_dirs=${library_dirs:+$library_dirs:}$library_dir
    done
    library_dirs=$library_dirs${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
    for library_dir in "$prefix"/lib/*-linux-* "$prefix"/usr/lib/*-linux-* "$prefix/lib" "$prefix/usr/lib"; do
        [ ! -d "$library_dir" ] || library_dirs=$library_dirs:$library_dir
    done
    LD_LIBRARY_PATH=$library_dirs
    if [ -f "$prefix/etc/fonts/pierlink.conf" ]; then
        FONTCONFIG_FILE=$prefix/etc/fonts/pierlink.conf
        FONTCONFIG_PATH=$prefix/etc/fonts
        export FONTCONFIG_FILE FONTCONFIG_PATH
    fi
    if [ ! -r /usr/share/X11/xkb/rules/evdev ] && [ -r "$prefix/usr/share/X11/xkb/rules/evdev" ]; then
        XKB_CONFIG_ROOT=$prefix/usr/share/X11/xkb
        export XKB_CONFIG_ROOT
    fi
    export PATH LD_LIBRARY_PATH XDG_DATA_DIRS XDG_CONFIG_DIRS
}
binary_environment() {
    binary_prefix=${1%/*}
    case "$1" in "$binroot"/*/usr/bin/*) binary_prefix=${1%/usr/bin/*};; esac
    if [ -f "$binary_prefix/.pierlink-created" ]; then prefix_environment "$binary_prefix"; fi
}
# 登录 shell 初始化之后逐项恢复，所有值经过相同的单引号转义。
session_environment() {
    for variable in PATH LD_LIBRARY_PATH XDG_DATA_DIRS XDG_CONFIG_DIRS FONTCONFIG_FILE FONTCONFIG_PATH XKB_CONFIG_ROOT XDG_CACHE_HOME XENVIRONMENT; do
        eval 'present=${'"$variable"'+yes}'
        if [ "$present" = yes ]; then
            eval 'value=${'"$variable"'}'
            printf 'export %s=%s; ' "$variable" "$(shell_quote "$value")"
        fi
    done
}
# 在启动会话之前生成固定资源；会话内所有终端共享，随桌面缓存一起清理。
light_xresources() {
    XENVIRONMENT=$XDG_CACHE_HOME/Xresources
    safe_path "$XENVIRONMENT"
    cat > "$XENVIRONMENT" <<'EOF' || fail permission_denied
XTerm*selectToClipboard: true
XTerm*VT100.translations: #override \n\
    Ctrl Shift <Key>V: insert-selection(CLIPBOARD) \n\
    Ctrl Shift <Key>C: copy-selection(CLIPBOARD) \n\
    Ctrl ~Shift <Key>v: insert-selection(CLIPBOARD)
EOF
    chmod 600 -- "$XENVIRONMENT" || fail permission_denied
    export XENVIRONMENT
}
xml_escape() { printf %s "$1" | sed 's/\&/\&amp;/g;s/</\&lt;/g;s/>/\&gt;/g;s/"/\&quot;/g;s/'"'"'/\&apos;/g'; }
# 常见发行版的 xkbcomp 路径编译为 /usr/bin；PATH 无法覆盖这个绝对路径。
keyboard_environment() {
    xkb_args=
    xkb_namespace=false
    if [ ! -x /usr/bin/xkbcomp ] || [ "${force_xkb_namespace:-0}" = 1 ]; then
        # 已装的 Xvnc 也可复用此前获取的键盘工具及其库、数据。
        if [ ! -x "${xkb_prefix_compiler:-}" ]; then
            for marker in "$binroot"/*/.pierlink-created "$HOME"/.local/share/pierlink/vnc-light/*/.pierlink-created; do
                [ -f "$marker" ] || continue
                keyboard_prefix=${marker%/.pierlink-created}
                [ -x "$keyboard_prefix/usr/bin/xkbcomp" ] || continue
                safe_path "$keyboard_prefix"
                prefix_environment "$keyboard_prefix"
                break
            done
        fi
        [ -x "${xkb_prefix_compiler:-}" ] || fail xkb_unavailable '系统和安装前缀均没有可用的 xkbcomp'
        xkb_namespace_tools || fail xkb_unavailable "$xkb_reason"
        xkb_namespace=true
    fi
    # 查找备用前缀可能补充了键盘数据，必须在前缀确定后构造参数。
    if [ -n "${XKB_CONFIG_ROOT:-}" ]; then
        # X server 内部会把此路径放进双引号 shell 命令，拒绝能二次解释的字符。
        case "$XKB_CONFIG_ROOT" in *'"'*|*'$'*|*'`'*|*'\'*|*'
'*) fail xkb_unavailable 'XKB 数据路径含不受支持的字符';; esac
        xkb_args="-xkbdir $(shell_quote "$XKB_CONFIG_ROOT")"
    fi
    [ -r /usr/share/X11/xkb/rules/evdev ] || [ -n "${XKB_CONFIG_ROOT:-}" ] || fail xkb_unavailable '没有可用的 XKB 键盘数据'
    export xkb_args xkb_namespace xkb_prefix_compiler
}
