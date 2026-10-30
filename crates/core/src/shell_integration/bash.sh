# 不安装 DEBUG 陷阱；历史编号未变化时不记录空回车或被历史设置忽略的输入。
__pl_pre() {
    local code=$? current=${HISTCMD-0} line
    __pl_status=$code
    if [[ -n ${__pl_seen-} && $current != "${__pl_history-}" ]]; then
        printf '\033]133;D;%s\a' "$code"
        line=$(HISTTIMEFORMAT= builtin history 1)
        if [[ $line =~ ^[[:space:]]*[0-9]+'  '(.*)$ ]]; then
            __pl_record "$code" "${BASH_REMATCH[1]}"
        fi
    fi
    __pl_history=$current
    __pl_seen=1
    __pl_cwd
    return "$code"
}
__pl_post() {
    case ${PS1-} in
        '\[\e]133;A\a\]'*'\[\e]133;B\a\]') ;;
        *) PS1='\[\e]133;A\a\]'${PS1-}'\[\e]133;B\a\]' ;;
    esac
    # 用户钩子可能改变退出码；提示符展开仍需看到用户命令的结果。
    return "${__pl_status-0}"
}
# 重复加载时不叠加入口；以换行分隔，兼容用户钩子末尾的注释。
if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == 'declare -'*a*' PROMPT_COMMAND='* ]]; then
    __pl_commands=()
    for __pl_command in "${PROMPT_COMMAND[@]}"; do
        case $__pl_command in __pl_pre|__pl_post) ;; *) __pl_commands+=("$__pl_command") ;; esac
    done
    PROMPT_COMMAND=(__pl_pre "${__pl_commands[@]}" __pl_post)
    unset __pl_commands __pl_command
else
    case $'\n'${PROMPT_COMMAND-}$'\n' in
        *$'\n__pl_pre\n'*) ;;
        *) PROMPT_COMMAND=$'__pl_pre\n'${PROMPT_COMMAND-}$'\n__pl_post' ;;
    esac
fi
# 子 shell 没有这些函数，不能继承钩子；它会通过自己的 rc 重新配置提示符。
export -n PROMPT_COMMAND
__pl_history=${HISTCMD-0}
__pl_seen=
if [[ -n ${BLE_VERSION-} ]]; then
    printf '\033]%s;i;native=1\a' "$__pl_osc"
else
    printf '\033]%s;i;native=0\a' "$__pl_osc"
fi
