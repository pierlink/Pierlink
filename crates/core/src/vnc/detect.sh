#!/bin/sh
# 采样并临时探测挂载能力；不读取密码文件、X cookie 或进程环境。
LC_ALL=C
export LC_ALL
exec 2>/dev/null
proc_root=/proc
xsessions=${PIERLINK_VNC_XSESSIONS_DIR:-/usr/share/xsessions}
os_release=/etc/os-release
state_dir=$HOME/.local/state/pierlink/vnc
uid=$(id -u) || exit 1

json_stream() {
    LC_ALL=C awk 'BEGIN { printf "\""; for(i=1;i<32;i++) ctl[sprintf("%c",i)]=i }
    { if(NR>1) printf "\\n"; for(i=1;i<=length($0);i++) {
        c=substr($0,i,1)
        if(c=="\"") printf "\\\""; else if(c=="\\") printf "\\\\"
        else if(c in ctl) printf "\\u%04x",ctl[c]; else printf "%s",c
    }} END { printf "\"" }'
}
value() { printf %s "$1" | json_stream; }
has() { if command -v "$1" >/dev/null 2>&1; then printf true; else printf false; fi; }
file() { if [ -r "$1" ]; then cat "$1" | json_stream; else printf '""'; fi; }
# 外部查询最多三秒，退出时同时回收计时进程。
detect_bounded() (
    "$@" </dev/null & job=$!
    ( sleep 3 & timer=$!; trap 'kill "$timer" 2>/dev/null; wait "$timer"; exit' TERM HUP INT
      wait "$timer"; kill -KILL "$job" 2>/dev/null ) >/dev/null 2>&1 & watchdog=$!
    trap 'kill -KILL "$job" 2>/dev/null; kill "$watchdog" 2>/dev/null; wait; exit 1' HUP INT TERM
    wait "$job"; result=$?
    kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
    exit "$result"
)
printf '{"arch":'; value "$(uname -m)"
printf ',"glibc":'; value "$(getconf GNU_LIBC_VERSION)"
printf ',"os_release":'; file "$os_release"
printf ',"package_manager":'
if command -v apt-get >/dev/null 2>&1; then printf '"Apt"'
elif command -v dnf >/dev/null 2>&1; then printf '"Dnf"'; else printf '"None"'; fi
printf ',"apt_available":'
if command -v apt-get >/dev/null && command -v apt-cache >/dev/null && command -v dpkg-query >/dev/null && command -v dpkg-deb >/dev/null; then printf true; else printf false; fi
printf ',"binary_path":'
binary=
binary_version=
for candidate in "$(command -v Xtigervnc)" "$(command -v Xvnc)" "$HOME"/.local/share/pierlink/vnc/*/Xtigervnc "$HOME"/.local/share/pierlink/vnc/*/Xvnc "$HOME"/.local/share/pierlink/vnc/*/usr/bin/Xtigervnc "$HOME"/.local/share/pierlink/vnc/*/usr/bin/Xvnc; do
    [ -x "$candidate" ] || continue
    version=$(binary_environment "$candidate"; detect_bounded "$candidate" -version 2>&1) || continue
    case "$version" in *TigerVNC*)
        valid=$(printf %s "$version" | awk 'match($0,/[0-9]+\.[0-9]+/) {v=substr($0,RSTART,RLENGTH);split(v,a,".");if(a[1]>1 || (a[1]==1 && a[2]>=9)) print "yes";exit}')
        [ "$valid" = yes ] || continue
        binary=$candidate; binary_version=$version; break;;
    esac
done
value "$binary"
printf ',"binary_version":'
value "$binary_version"
printf ',"libraries":'; value "$(detect_bounded /sbin/ldconfig -p || detect_bounded ldconfig -p)"
printf ',"tools":{'
comma=
for tool in setsid dbus-run-session dbus-launch xauth mcookie socat python3 perl loginctl busctl curl wget; do
    printf '%s' "$comma"; value "$(printf %s "$tool" | tr '-' '_')"; printf ':'; has "$tool"; comma=,
done
printf ',"platform_python":'; if [ -x /usr/libexec/platform-python ]; then printf true; else printf false; fi
printf ',"xkbcomp":'; if [ -x /usr/bin/xkbcomp ]; then printf true; else printf false; fi
printf ',"xkb_rules":'; if [ -r /usr/share/X11/xkb/rules/evdev ]; then printf true; else printf false; fi
printf '},"xkb_namespace":{"available":'
if xkb_reason=$(xkb_namespace_probe); then printf true; else printf false; fi
printf ',"reason":'; if [ -n "$xkb_reason" ]; then value "$xkb_reason"; else printf null; fi
printf ',"install_command":'; value "$(xkb_install_command)"
printf '},"kill_user_processes":'; value "$(detect_bounded busctl get-property org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager KillUserProcesses)"
printf ',"linger":'; value "$(detect_bounded loginctl show-user "$uid" -p Linger --value)"
printf ',"sessions":['
comma=
for proc in "$proc_root"/[0-9]*; do
    [ -r "$proc/status" ] || continue
    owner=$(awk '$1=="Uid:" {print $2; exit}' "$proc/status")
    [ "$owner" = "$uid" ] || continue
    comm=$(cat "$proc/comm")
    case "$comm" in
        Xvnc|Xtigervnc|x0vncserver|x11vnc|vncserver-x11-core|vncserver-virtuald|wayvnc) :;;
        vncserver-x11-c*|vncserver-virt*)
            executable=$(readlink "$proc/exe"); comm=${executable##*/}
            case "$comm" in vncserver-x11-core|vncserver-virtuald) :;; *) continue;; esac;;
        *) continue;;
    esac
    # 通过十进制字节保留 NUL 边界；从不把其他参数（如口令）写进输出。
    selected=$(od -An -v -tu1 "$proc/cmdline" | awk '
      function arg() {
        if(s ~ /^:[0-9]+$/) display=s
        if(previous=="-SecurityTypes") security=s
        if(s ~ /^-SecurityTypes=/) { security=s; sub(/^-SecurityTypes=/,"",security) }
        previous=s; s=""
      }
      { for(i=1;i<=NF;i++) { if($i==0) arg(); else s=s sprintf("%c",$i) } }
      END { if(length(s)) arg(); printf "%s\n%s",display,security }')
    display=$(printf '%s\n' "$selected" | sed -n '1p')
    security=$(printf '%s\n' "$selected" | sed '1d')
    inodes=$(
        for fd in "$proc"/fd/*; do
            link=$(readlink "$fd")
            case "$link" in socket:\[*\]) inode=${link#socket:[}; printf '%s\n' "${inode%]}";; esac
        done
    )
    printf '%s{"pid":%s,"comm":' "$comma" "${proc##*/}"; value "$comm"
    printf ',"display_arg":'; value "$display"
    printf ',"security_arg":'; value "$security"
    printf ',"version":'
    case "$comm" in Xvnc) value "$(detect_bounded "$proc/exe" -version 2>&1)";; *) printf '""';; esac
    printf ',"cgroup":'; file "$proc/cgroup"
    printf ',"tcp":'; awk -v ids="$inodes" 'BEGIN {split(ids,a);for(i in a) wanted[a[i]]=1} $10 in wanted && $4=="0A"' "$proc_root/net/tcp" | json_stream
    printf ',"tcp6":'; awk -v ids="$inodes" 'BEGIN {split(ids,a);for(i in a) wanted[a[i]]=1} $10 in wanted && $4=="0A"' "$proc_root/net/tcp6" | json_stream
    printf ',"unix":'; awk -v ids="$inodes" 'BEGIN {split(ids,a);for(i in a) wanted[a[i]]=1} $7 in wanted' "$proc_root/net/unix" | json_stream
    printf '}'; comma=,
done
printf '],"desktops":['
comma=
for desktop in "$xsessions"/*.desktop; do
    [ -r "$desktop" ] || continue
    # Desktop Entry 的 Exec 不是 shell；只拆第一个带引号的可执行文件名，不执行它。
    executable=$(awk '
        /^\[/ {active=($0=="[Desktop Entry]")}
        active && /^Exec=/ {
            s=substr($0,6); sub(/^[ \t]+/,"",s); quoted=0; escaped=0
            for(i=1;i<=length(s);i++) {
                c=substr(s,i,1)
                if(escaped) {printf "%s",c; escaped=0}
                else if(c=="\\") escaped=1
                else if(c=="\"") quoted=!quoted
                else if(!quoted && c ~ /[ \t]/) break
                else printf "%s",c
            }
            exit
        }' "$desktop")
    id=${desktop##*/}; id=${id%.desktop}
    printf '%s{"id":' "$comma"; value "$id"
    printf ',"content":'; file "$desktop"
    printf ',"executable":'; if [ -n "$executable" ] && command -v "$executable" >/dev/null 2>&1; then printf true; else printf false; fi
    printf '}'; comma=,
done
printf '],"pierlink_states":'
printf '[{PIERLINK_STATES}]'
printf '}'
