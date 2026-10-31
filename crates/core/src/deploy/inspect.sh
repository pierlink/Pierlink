user=$(id -un)
machine=${PIERLINK_TEST_MACHINE_ID:-}
if [ -z "$machine" ]; then
    for file in /etc/machine-id /var/lib/dbus/machine-id "$state/machine-id"; do
        if [ -s "$file" ]; then machine=$(cat "$file"); break; fi
    done
fi
if [ -z "$machine" ]; then
    acquire
    safe_path "$state/machine-id"
    if [ ! -s "$state/machine-id" ]; then
        od -An -N32 -tx1 /dev/urandom | tr -d ' \n' > "$state/machine-id"
    fi
    machine=$(cat "$state/machine-id")
fi
instance_key=$(printf '%s\n%s' "$machine" "$user" | sha256_of | cut -c1-16)
arch=$(uname -m)
arch_supported=null
case "$arch" in x86_64) arch_supported='"X86_64"';; aarch64) arch_supported='"Aarch64"';; esac
os_name=$(sed -n 's/^PRETTY_NAME=//p' /etc/os-release 2>/dev/null | head -n 1 | sed 's/^"//;s/"$//')
fs_type=$(stat -f -c %T "$HOME")
watches=$(cat /proc/sys/fs/inotify/max_user_watches 2>/dev/null || printf null)
free=$(df -Pk "$HOME" | awk 'NR==2 {printf "%.0f",$4*1024}')
[ -n "$free" ] || free=null
# 用自动清理的小脚本检测执行权限，不留下程序或配置目录。
probe_dir=$HOME
if [ -d "$binroot" ]; then probe_dir=$binroot; fi
temporary=$(mktemp "$probe_dir/.pierlink-noexec-XXXXXXXX") || fail permission_denied
printf '#!/bin/sh\nexit 0\n' > "$temporary"
chmod 700 "$temporary"
noexec=false
"$temporary" >/dev/null 2>&1 || noexec=true
rm -f -- "$temporary"; temporary=
instance=null
instance_id=
config=
if [ -f "$state/instances/$instance_key" ]; then
    instance_id=$(cat "$state/instances/$instance_key")
    case "$instance_id" in *[!a-f0-9-]*|'') fail instance_changed;; esac
    instance_dir=$state/$instance_id
    if [ -f "$instance_dir/info.json" ]; then
        config=$(cat "$instance_dir/home/config.xml" 2>/dev/null || :)
        running=false
        for proc in /proc/[0-9]*/cmdline; do
            if tr '\000' '\n' < "$proc" 2>/dev/null | grep -Fx -- "--home=$instance_dir/home" >/dev/null; then running=true; break; fi
        done
        instance=$(printf '{"info":'; cat "$instance_dir/info.json"; printf ',"running":%s,"devices":' "$running"; printf '[]'; printf '}')
    fi
fi
own=0
for proc in /proc/[0-9]*/comm; do
    [ "$(cat "$proc" 2>/dev/null || :)" = syncthing ] || continue
    [ "$(stat -c %u "$proc" 2>/dev/null || :)" = "$(id -u)" ] || continue
    if ! tr '\000' '\n' < "${proc%/comm}/cmdline" 2>/dev/null | grep -F -- "--home=$state/" >/dev/null; then own=$((own+1)); fi
done
printf '{"user":'; json "$user"; printf ',"home":'; json "$HOME"
printf ',"arch":'; json "$arch"; printf ',"arch_supported":%s,"instance_key":' "$arch_supported"; json "$instance_key"
printf ',"os_name":'; if [ -n "$os_name" ]; then json "$os_name"; else printf null; fi
printf ',"instance":%s,"own_syncthing_running":%s,"systemd":' "$instance" "$own"; json "$systemd"
printf ',"linger":%s,"kill_user_processes":%s,"home_noexec":%s,"home_fs_type":' "$linger" "$kill_users" "$noexec"; json "$fs_type"
printf ',"inotify_max_user_watches":%s,"free_bytes":%s,"runtime_dir":' "$watches" "$free"
if [ -n "$runtime" ]; then json "$runtime"; else printf null; fi
printf ',"lock":'
if [ -d "$lock" ] && [ "$locked" = false ]; then
    age=$(($(date +%s)-$(stat -c %Y "$lock")))
    [ "$age" -ge 0 ] || age=0
    printf '{"owner":'; json "$(cat "$lock/owner" 2>/dev/null || printf '未知')"; printf ',"age_seconds":%s}' "$age"
else printf null; fi
printf ',"pending_instance_id":'; if [ "$instance" = null ] && [ -n "$instance_id" ]; then json "$instance_id"; else printf null; fi
printf ',"config":'; json "$config"; printf '}'
