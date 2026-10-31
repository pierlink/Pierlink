set -eu
umask 077
sshdir="$HOME/.ssh"
# 只有安装才需要建 ~/.ssh；查看列表是只读操作，不建目录、不加锁（不会被残留的锁挡住）
if [ "$action" = install ] && [ ! -d "$sshdir" ]; then mkdir -m 700 "$sshdir" 2>/dev/null || [ -d "$sshdir" ]; fi
file="$sshdir/authorized_keys"
lock="$sshdir/.pierlink-lock"
# 移除时连授权文件都没有：没什么可删，也不必加锁（~/.ssh 可能不存在，锁建不起来）
if [ "$action" = remove ] && [ ! -e "$file" ] && [ ! -L "$file" ]; then printf '{"status":"not_found"}'; exit 0; fi
if [ "$action" != list ]; then
i=0
while ! mkdir "$lock" 2>/dev/null; do
    # GNU stat 和 busybox stat 均支持秒级修改时间；无法读取时不冒险清锁。
    if [ -d "$lock" ] && [ ! -L "$lock" ]; then
        modified=$(stat -c %Y "$lock" 2>/dev/null) || modified=0
        now=$(date +%s)
        case "$modified" in ''|*[!0-9]*) modified=0;; esac
        if [ "$modified" -gt 0 ] && [ "$((now-modified))" -gt 600 ]; then
            # 先原子改名再删除：别人刚建的新锁不会被误删（改名失败说明已被别人处理，继续等）
            stale="$lock.stale.$$"
            if mv "$lock" "$stale" 2>/dev/null; then rm -rf "$stale"; continue; fi
        fi
    fi
    i=$((i+1)); [ "$i" -lt 11 ] || exit 71
    sleep 1
done
trap 'rm -f "$lock/new"; rmdir "$lock"' EXIT
fi
trap 'exit 72' HUP INT TERM
if [ "$action" = remove ] && [ -L "$file" ]; then printf '{"status":"symlink"}'; exit 0; fi
if [ ! -e "$file" ]; then
    if [ "$action" = install ]; then
        [ ! -L "$file" ] || exit 73
        (set -C; : > "$file")
        chmod 600 "$file"
    else
        printf '{"status":"not_found","hex":""}'; exit 0
    fi
fi
[ -f "$file" ] || exit 74
# 只观察已有权限；不替用户修正。
warning=false
for p in "$sshdir" "$file"; do
    mode=$(stat -c %a "$p") || exit 75
    case "$mode" in *00) ;; *) warning=true ;; esac
done
if [ "$action" = list ]; then
    printf '{"hex":"'
    od -An -v -tx1 "$file" | tr -d ' \n'
    printf '"}'
    exit 0
fi
non_default=false
if [ -r /etc/ssh/sshd_config ] && awk '
tolower($1)=="authorizedkeysfile" && $2!=".ssh/authorized_keys" { found=1 }
END { exit !found }
' /etc/ssh/sshd_config; then non_default=true; fi
newline=true
if [ -s "$file" ] && [ "$(tail -c 1 "$file" | od -An -tu1 | tr -d ' ')" != 10 ]; then newline=false; fi
# 识别带引号和反斜杠的 options 字段，只比较前两个公钥字段。
matched=$(awk -v kt="$kind" -v kd="$data" -v action="$action" -v newline="$newline" -v out="$lock/new" '
function word(   c,s,q,e) {
    sub(/^[ \t]+/, "", rest); s=""; q=0; e=0
    while (length(rest)) {
        c=substr(rest,1,1); rest=substr(rest,2)
        if (!q && !e && c ~ /[ \t]/) break
        s=s c
        if (e) e=0
        else if (c=="\\") e=1
        else if (c=="\"") q=!q
    }
    return s
}
{
    rest=$0; a=word()
    if (a !~ /^(ssh-|ecdsa-|sk-)/) a=word()
    b=word(); same=(a==kt && b==kd && $0 !~ /^[ \t]*#/)
    last_same=same
    if (same) count++
    else if (action=="remove") {
        printf "%s%s", (kept ? "\n" : ""), $0 > out
        kept=1
    }
}
END {
    if (kept && (last_same || newline=="true")) printf "\n" > out
    print count+0
}
' "$file")
if [ "$action" = install ]; then
    if [ "$matched" -gt 0 ]; then status=already_present
    else
        if [ "$newline" = false ]; then printf '\n' >> "$file"; fi
        printf '%s\n' "$line" >> "$file"
        status=installed
    fi
    printf '{"status":"%s","permissions_warning":%s,"non_default_authorized_keys":%s}' "$status" "$warning" "$non_default"
else
    if [ "$matched" -eq 0 ]; then printf '{"status":"not_found"}'; exit 0; fi
    [ -e "$lock/new" ] || : > "$lock/new"
    mode=$(stat -c %a "$file")
    owner=$(stat -c %u:%g "$file")
    chown "$owner" "$lock/new"
    chmod "$mode" "$lock/new"
    mv -f "$lock/new" "$file"
    printf '{"status":"removed","count":%s}' "$matched"
fi
