# 安装完整桌面前的只读检查：管理员权限、内存和 CPU、要写入的文件系统、要装的软件包大小。不安装、不写任何文件。
# 前面由 Rust 给出 manager（apt 或 dnf）、packages（已校验的包名）、install_path、cache_path。
meminfo=/proc/meminfo
admin=none
if [ "$uid" = 0 ]; then admin=root
elif command -v sudo >/dev/null 2>&1; then
    # 只对管理员组里的账号试 sudo：sudoers 里没有的账号试一次会被记成安全事件、发邮件给管理员
    case " $(id -Gn) " in
        *" sudo "*|*" wheel "*|*" admin "*|*" google-sudoers "*)
            if bounded 5 sudo -n true >/dev/null 2>&1; then admin=nopass; else admin=password; fi;;
    esac
fi
printf '{"admin":"%s"' "$admin"
printf ',"memory":'; json "$(awk '$1 == "MemTotal:" {print $2}' "$meminfo")"
printf ',"available_memory":'; json "$(awk '$1 == "MemAvailable:" {print $2}' "$meminfo")"
printf ',"cpus":'; json "$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN)"
# 路径所在文件系统的可用 KiB 和挂载点；路径还不存在（缓存目录没建）时看最近的上一级
space() {
    target=$1
    while [ ! -e "$target" ]; do
        target=${target%/*}
        [ -n "$target" ] || target=/
    done
    df -Pk -- "$target" | awk 'NR == 2 {mount = $6; for (i = 7; i <= NF; i++) mount = mount " " $i; print $4; print mount}'
}
for role in install cache; do
    if [ "$role" = install ]; then target=$install_path; else target=$cache_path; fi
    result=$(space "$target")
    printf ',"%s":{"available":' "$role"; json "$(printf '%s\n' "$result" | sed -n 1p)"
    printf ',"mount":'; json "$(printf '%s\n' "$result" | sed -n 2p)"; printf '}'
done
# 只有 apt 能不用 root 模拟安装：先拿到要装的包，再按软件源索引累加装好后的大小（KiB）和下载大小（字节）。
# 索引没更新、找不到包时给 null，由 Rust 按固定估计算。
printf ',"estimate":'
plan=
if [ "$manager" = apt ] && command -v apt-get >/dev/null 2>&1 && command -v apt-cache >/dev/null 2>&1 &&
    plan=$(bounded 60 apt-get -s -o Debug::NoLocking=true install --no-install-recommends $packages); then
    names=$(printf '%s\n' "$plan" | awk '$1 == "Inst" {print $2}')
    count=$(printf '%s\n' "$names" | grep -c .)
    if [ "$count" = 0 ]; then sizes='0 0 0'
    else
        sizes=$(bounded 30 apt-cache show --no-all-versions $names | awk '
            /^Package:/ {n++} /^Installed-Size:/ {i += $2} /^Size:/ {s += $2}
            END {printf "%d %.0f %.0f", n, i, s}')
    fi
    set -- $sizes
    printf '{"packages":'; json "$count"
    printf ',"records":'; json "${1:-0}"
    printf ',"installed":'; json "${2:-0}"
    printf ',"download":'; json "${3:-0}"; printf '}'
else
    printf null
fi
printf '}'
