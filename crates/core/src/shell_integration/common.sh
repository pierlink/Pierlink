# URL 按字节编码；结果写入变量，提示符刷新不启动额外进程。
__pl_url() {
    local LC_ALL=C rest=${1-} ch hex
    __pl_url_value=
    while [ -n "$rest" ]; do
        ch=${rest%"${rest#?}"}
        rest=${rest#?}
        case $ch in
            [a-zA-Z0-9/._~-]) __pl_url_value=$__pl_url_value$ch ;;
            *) printf -v hex '%%%02X' "'$ch"; __pl_url_value=$__pl_url_value$hex ;;
        esac
    done
}
__pl_cwd() {
    __pl_url "${HOSTNAME-${HOST-}}"
    local host=$__pl_url_value
    __pl_url "${PWD-}"
    printf '\033]7;file://%s%s\a' "$host" "$__pl_url_value"
}
__pl_record() {
    local code=$1 command=$2 encoded
    [ "${#command}" -le 1000 ] || return 0
    case $command in ''|*[[:cntrl:]]*) return 0 ;; esac
    local LC_ALL=C
    case $command in *[[:cntrl:]]*) return 0 ;; esac
    encoded=$(printf '%s' "$command" | base64 2>/dev/null) || return 0
    encoded=${encoded//$'\n'/}
    case $encoded in ''|*[!A-Za-z0-9+/=]*) return 0 ;; esac
    # 编号、分隔符及退出码也计入 4 KiB 的 payload 上限。
    [ "$(( ${#encoded} + ${#code} + ${#__pl_osc} + 4 ))" -le 4096 ] || return 0
    printf '\033]%s;c;%s;%s\a' "$__pl_osc" "$code" "$encoded"
}
