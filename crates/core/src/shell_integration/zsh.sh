__pl_exec() {
    if [[ -o histignorespace && $1 == ' '* ]]; then
        unset __pl_command
    else
        __pl_command=$1
    fi
}
__pl_pre() {
    local code=$?
    # 即时提示符捕获期间保持静默，初始化及目录报告延后到恢复输出后的提示符。
    (( ${+__p9k_instant_prompt_active} )) && return 0
    if [[ -z ${__pl_initialized-} ]]; then
        if (( $+functions[_zsh_autosuggest_start] || $+ZSH_AUTOSUGGEST_STRATEGY )); then
            printf '\033]%s;i;native=1\a' "$__pl_osc"
        else
            printf '\033]%s;i;native=0\a' "$__pl_osc"
        fi
        __pl_initialized=1
    fi
    if (( ${+__pl_command} )); then
        printf '\033]133;D;%s\a' "$code"
        __pl_record "$code" "$__pl_command"
        unset __pl_command
    fi
    __pl_cwd
    # zsh 的 precmd 钩子返回非零会阻断其余钩子，因此处理完后返回成功。
    return 0
}
__pl_post() {
    (( ${+__p9k_instant_prompt_active} )) && return 0
    case ${PS1-} in
        $'%{\e]133;A\a%}'*$'%{\e]133;B\a%}') ;;
        *) PS1=$'%{\e]133;A\a%}'${PS1-}$'%{\e]133;B\a%}' ;;
    esac
}
typeset -ga precmd_functions preexec_functions
precmd_functions=(__pl_pre ${precmd_functions:#(__pl_pre|__pl_post)} __pl_post)
preexec_functions=(${preexec_functions:#__pl_exec} __pl_exec)
