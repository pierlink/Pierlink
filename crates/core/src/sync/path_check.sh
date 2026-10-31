# 所有目标路径均经 sh_quote 传入；只读取，不创建探针或修改 Git 排除文件。
set -eu
exec 2>/dev/null
export LC_ALL=C
fail() { printf '{"error":true}'; exit 0; }
json() {
    printf '%s' "$1" | od -An -v -tu1 | awk 'BEGIN { printf "\"" } { for(i=1;i<=NF;i++) { n=$i; if(n==34 || n==92) printf "\\%c",n; else if(n<32) printf "\\u%04x",n; else printf "%c",n } } END { printf "\"" }'
}
case "$target" in '~') target=$HOME;; '~/'*) target=$HOME/${target#\~/};; /*) ;; *) fail;; esac
canonical=$(realpath -m -- "$target") || fail
home=$(realpath -e -- "$HOME") || fail
parent=$canonical
while [ ! -e "$parent" ]; do parent=${parent%/*}; [ -n "$parent" ] || parent=/; done
[ -d "$parent" ] || fail
filesystem=$(stat -f -c %T -- "$parent") || fail
printf '{"canonical_path":'; json "$canonical"
printf ',"home":'; json "$home"
printf ',"filesystem":'; json "$filesystem"
printf ',"marker_present":'; if [ -e "$canonical/.stfolder" ]; then printf true; else printf false; fi
printf ',"has_ignore_file":'; if [ -e "$canonical/.stignore" ]; then printf true; else printf false; fi
printf ',"git_repository":'; if [ -e "$canonical/.git" ]; then printf true; else printf false; fi
printf '}'
