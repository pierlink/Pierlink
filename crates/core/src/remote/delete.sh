# 快速删除：rm -rf 删掉参数里的路径（不跟随符号链接）。只输出一行 JSON，不输出服务器原文：
# {"left":null} 全删掉了；{"left":"还在的第一个路径","reason":"permission|readonly|busy|other"} 有删不掉的。
export LC_ALL=C
# 只留前几行报错用来判断原因；awk 读完全部输出，rm 不会因为管道关了半路退出
err=$(rm -rf -- "$@" 2>&1 >/dev/null | awk 'NR <= 5')
left=
for p in "$@"; do
    if [ -e "$p" ] || [ -L "$p" ]; then
        left=$p
        break
    fi
done
if [ -z "$left" ]; then
    printf '{"left":null}'
    exit 0
fi
case $err in
    *'Permission denied'*|*'Operation not permitted'*) reason=permission ;;
    *'Read-only file system'*) reason=readonly ;;
    *'busy'*) reason=busy ;;
    *) reason=other ;;
esac
printf '{"left":'
printf '%s' "$left" | od -An -v -tu1 | awk 'BEGIN {printf "\""} {for(i=1;i<=NF;i++) {n=$i; if(n==34 || n==92) printf "\\%c",n; else if(n<32) printf "\\u%04x",n; else printf "%c",n}} END {printf "\""}'
printf ',"reason":"%s"}' "$reason"
exit 0
