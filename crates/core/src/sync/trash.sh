# 路径由统一转义函数传入；不跟随符号链接，不使用递归 rm，不记录文件名。
exec 2>/dev/null
LC_ALL=C; export LC_ALL
fail() { printf '{"files":0,"bytes":0,"error":"%s"}' "$1"; }
safe_path() {
    case "$1" in /*) ;; *) return 1;; esac
    rest=${1#/}; current=
    while [ -n "$rest" ]; do
        part=${rest%%/*}
        case "$part" in ''|.|..) return 1;; esac
        current=$current/$part
        [ ! -L "$current" ] || return 1
        if [ -e "$current" ] && [ ! -d "$current" ]; then return 1; fi
        case "$rest" in */*) rest=${rest#*/};; *) rest=;; esac
    done
}
target=$root/$id
safe_path "$target" || { fail unsafe_path; exit 0; }
# 带哨兵保留路径末尾换行。
canonical=$(realpath -m -- "$target" && printf '.') || { fail io; exit 0; }
canonical=${canonical%.}
# realpath 输出的最后一个换行单独移除。
canonical=${canonical%'
'}
[ "$canonical" = "$target" ] || { fail unsafe_path; exit 0; }
check_sync() {
    case "$1" in /*) ;; *) fail unsafe_path; return 1;; esac
    sync_path=$(realpath -m -- "$1" && printf '.') || { fail io; return 1; }
    sync_path=${sync_path%.}; sync_path=${sync_path%'
'}
    [ "$sync_path" != / ] || { fail unsafe_path; return 1; }
    case "$canonical/" in "$sync_path/"*) fail unsafe_path; return 1;; esac
    case "$sync_path/" in "$canonical/"*) fail unsafe_path; return 1;; esac
}
walk() (
    expected=$1
    safe_path "$expected" || { printf 'unsafe_path\n'; exit; }
    cd -P -- "$expected" || { printf 'io\n'; exit; }
    actual=$(pwd -P && printf '.'); actual=${actual%.}; actual=${actual%'
'}
    [ "$actual" = "$expected" ] || { printf 'unsafe_path\n'; exit; }
    [ -r . ] && [ -x . ] || { printf 'io\n'; exit; }
    for item in ./* ./.[!.]* ./..?*; do
        [ ! -L "$item" ] || { printf 'unsafe_path\n'; continue; }
        [ -e "$item" ] || continue
        if [ -d "$item" ]; then
            walk "$expected/${item#./}"
            if [ "$clear" = 1 ]; then rmdir -- "$item" || :; fi
        elif [ -f "$item" ]; then
            size=$(stat -c %s -- "$item") || { [ ! -e "$item" ] || printf 'io\n'; continue; }
            case "$size" in ''|*[!0-9]*) printf 'io\n'; continue;; esac
            if [ "$clear" = 1 ]; then
                rm -- "$item" || { [ ! -e "$item" ] || printf 'io\n'; continue; }
            fi
            printf '1 %s\n' "$size"
        else printf 'unsafe_path\n'
        fi
    done
)
run_trash() {
    if [ ! -e "$target" ]; then printf '{"files":0,"bytes":0}'; return; fi
    [ -d "$target" ] || { fail unsafe_path; return; }
    walk "$canonical" | awk '
        NF==2 && $1==1 && $2~/^[0-9]+$/ { files++; bytes+=$2; next }
        { error=$0 }
        END {
            printf "{\"files\":%.0f,\"bytes\":%.0f",files,bytes
            if(error!="") printf ",\"error\":\"%s\"",error=="unsafe_path" ? error : "io"
            printf "}"
        }'
}
