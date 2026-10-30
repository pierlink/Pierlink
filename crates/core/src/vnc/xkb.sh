# 仅覆盖 Xvnc 所在命名空间的编译期工具目录，不隔离网络和 /tmp。
xkb_install_command() {
    if command -v apt-get >/dev/null; then printf 'sudo apt-get install -y x11-xkb-utils'
    elif command -v dnf >/dev/null; then printf 'sudo dnf install -y xorg-x11-xkb-utils'
    else printf 'sudo apt-get install -y x11-xkb-utils'; fi
}
xkb_namespace_tools() {
    xkb_unshare=$(command -v unshare) || { xkb_reason='没有 unshare，无法创建用户命名空间'; xkb_reason_key=vnc.xkb_unshare; return 1; }
    command -v mount >/dev/null || { xkb_reason='没有 mount，无法挂载键盘工具目录'; xkb_reason_key=vnc.xkb_mount; return 1; }
    export xkb_unshare
}
xkb_namespace_reason() {
    case "$(cat "$1/stage" 2>/dev/null)" in
        mount) xkb_reason_key=vnc.xkb_overlay; printf '无法挂载 overlay；内核或安全策略不允许，或私有目录所在文件系统不支持 overlay upper';;
        ready) xkb_reason_key=vnc.xkb_probe_exit; printf '命名空间挂载完成，但探测进程未能正常退出';;
        *) xkb_reason_key=vnc.xkb_namespace; printf '无法创建用户和挂载命名空间；内核或 AppArmor 等安全策略不允许';;
    esac
}
xkb_namespace_remove() {
    # overlay 内部工作目录可能是 000；退出命名空间后恢复权限以便整体回收。
    if [ -d "$1/work/work" ] && [ ! -L "$1/work" ] && [ ! -L "$1/work/work" ]; then
        chmod u+rwx -- "$1/work/work" 2>/dev/null || :
    fi
    rm -rf -- "$1"
}
xkb_namespace_prepare() {
    xkb_reason='无法创建键盘工具的私有目录'; xkb_reason_key=vnc.xkb_private_directory
    mkdir -m 700 -- "$1" || return 1
    if mkdir -m 700 -- "$1/upper" "$1/work" &&
        ln -s -- "$xkb_prefix_compiler" "$1/upper/xkbcomp" &&
        printf namespace > "$1/stage"; then return 0; fi
    xkb_namespace_remove "$1"
    return 1
}
# 路径以位置参数传递；先切换目录，让 overlay 选项只含固定相对路径。
# 这样空格、引号、逗号、冒号和反斜杠都不会进入 mount 的选项解析器。
xkb_namespace_script='
        dir=$1; mode=$2; shift 2
        previous=$PWD
        cd -- "$dir" || exit 1
        printf mount > stage || exit 1
        mount -t overlay overlay -o lowerdir=/usr/bin,upperdir=upper,workdir=work /usr/bin || exit 1
        printf ready > stage || exit 1
        [ "$mode" != probe ] || exit 0
        cd -- "$previous" || exit 1
        exec "$@"
    '
xkb_namespace_exec() {
    exec "$xkb_unshare" -Ur -m --propagation private /bin/sh -c "$xkb_namespace_script" sh "$@"
}
# 探测只创建短期目录并尝试挂载，不启动 Xvnc、不运行前缀程序。
xkb_namespace_probe() (
    xkb_namespace_tools || { printf %s "$xkb_reason"; exit 1; }
    parent=$state_dir
    while [ ! -d "$parent" ] && [ "$parent" != "$HOME" ]; do parent=${parent%/*}; done
    probe=$(mktemp -d "$parent/.xkb-probe.XXXXXXXX") || { printf '无法在桌面所在文件系统创建探测目录'; exit 1; }
    trap 'xkb_namespace_remove "$probe"' 0
    trap 'exit 1' HUP INT TERM
    mkdir -m 700 -- "$probe/upper" "$probe/work" && printf namespace > "$probe/stage" || {
        printf '无法创建 overlay 探测目录'; exit 1;
    }
    if bounded 5 "$xkb_unshare" -Ur -m --propagation private /bin/sh -c "$xkb_namespace_script" sh "$probe" probe >/dev/null 2>&1; then
        exit 0
    fi
    xkb_namespace_reason "$probe"
    exit 1
)
