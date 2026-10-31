//! 终端预测标记及一次性启动文件；不修改用户配置，不保存命令历史副本。
use crate::{error::AppError, ssh::quote::sh_quote};
use ssh_key::encoding::base64::{Base64, Encoding};

/// shell 与前端约定的私有 OSC 编号。
pub(crate) const PRIVATE_OSC: u16 = 7701;
const COMMON: &str = include_str!("common.sh");
const BASH: &str = include_str!("bash.sh");
const ZSH: &str = include_str!("zsh.sh");
const CLEANUP: &str = r#"rm -f -- "$d/pl" "$d/bashrc" "$d/.zshenv" "$d/.zprofile" "$d/.zshrc" "$d/.zlogin"; rmdir -- "$d" 2>/dev/null"#;

/// 无参数；返回远端 Bash 与本机 Git Bash 共用的提示符和历史记录钩子。
pub(crate) fn bash_hook() -> String {
    format!("__pl_osc={PRIVATE_OSC}\n{COMMON}\n{BASH}")
}

fn bash_rc() -> String {
    format!(
        "rm -f -- \"${{BASH_SOURCE[0]}}\"; rmdir -- \"${{BASH_SOURCE[0]%/*}}\" 2>/dev/null\n{}\n{}",
        crate::detect::shells::bash::LOGIN_RC,
        bash_hook()
    )
}

fn zsh_rc(name: &str) -> String {
    let mut script = String::new();
    if name == ".zshenv" {
        script.push_str(
            "__pl_dir=$ZDOTDIR\n__pl_zdotdir_set=${__PL_ZDOTDIR_SET-}\n__pl_zdotdir=${__PL_ZDOTDIR-}\nunset __PL_ZDOTDIR_SET __PL_ZDOTDIR\n",
        );
    }
    script.push_str(
        "if [[ -n $__pl_zdotdir_set ]]; then export ZDOTDIR=$__pl_zdotdir; else unset ZDOTDIR; fi\n",
    );
    if matches!(name, ".zshrc" | ".zlogin") {
        // 交互 shell 后续自然从用户目录读取 .zlogin；提前清理也覆盖用户 rc 中的 exec。
        script.push_str(&format!("() {{ local d=$__pl_dir; {CLEANUP}; }}\n"));
    }
    if name == ".zshrc" {
        // p10k 每次重建提示符，因此在加载主题前启用其原生标记；保留用户选择，不导出默认值。
        script.push_str(
            "if (( ! ${+POWERLEVEL9K_TERM_SHELL_INTEGRATION} )); then typeset -g POWERLEVEL9K_TERM_SHELL_INTEGRATION=true; fi\n",
        );
    }
    script.push_str(&format!(
        "if [[ -r ${{ZDOTDIR-$HOME}}/{name} ]]; then . \"${{ZDOTDIR-$HOME}}/{name}\"; fi\n"
    ));
    if name == ".zshrc" {
        script.push_str(&format!("__pl_osc={PRIVATE_OSC}\n{COMMON}\n{ZSH}"));
    }
    if matches!(name, ".zshenv" | ".zprofile") {
        script.push_str(
            "__pl_zdotdir_set=${ZDOTDIR+x}\n__pl_zdotdir=${ZDOTDIR-}\nexport ZDOTDIR=$__pl_dir\n",
        );
    } else {
        script.push_str("unset __pl_dir __pl_zdotdir_set __pl_zdotdir\n");
    }
    script
}

fn loader(shell: Option<&str>) -> Result<String, AppError> {
    let shell = match shell {
        Some(shell) => sh_quote(shell)?,
        None => "\"${SHELL:-/bin/sh}\"".into(),
    };
    let mut script = format!(
        "# 文件仅在启动期间存在；失败和信号均清理，成功后由 shell rc 接管。\n__pl_cleanup() {{ {CLEANUP}; }}\ntrap __pl_cleanup 0\ntrap 'exit 1' 1 2 15\nrm -f -- \"$d/pl\"\n__pl_shell={shell}\n__pl_start() {{\ncase ${{__pl_shell##*/}} in\nbash)\ncat 2>/dev/null >\"$d/bashrc\" <<'PIERLINK_BASH'\n{}\nPIERLINK_BASH\n[ \"$?\" -eq 0 ] || return\nexec \"$__pl_shell\" --rcfile \"$d/bashrc\" -i\n;;\nzsh)\n",
        bash_rc()
    );
    for name in [".zshenv", ".zprofile", ".zshrc", ".zlogin"] {
        script.push_str(&format!(
            "cat 2>/dev/null >\"$d/{name}\" <<'PIERLINK_ZSH'\n{}\nPIERLINK_ZSH\n[ \"$?\" -eq 0 ] || return\n",
            zsh_rc(name)
        ));
    }
    script.push_str("__PL_ZDOTDIR_SET=${ZDOTDIR+x}\n__PL_ZDOTDIR=${ZDOTDIR-}\nexport __PL_ZDOTDIR_SET __PL_ZDOTDIR\nZDOTDIR=$d\nexport ZDOTDIR\nexec \"$__pl_shell\" -l\n;;\nesac\n}\n__pl_start\n__pl_cleanup\ntrap - 0 1 2 15\nunset -f __pl_start __pl_cleanup\nunset __pl_shell\n");
    Ok(script)
}

/// 将已通过登录 shell 校验的路径包装为跨登录 shell 可解析的命令；无路径时使用远端 SHELL。
pub(crate) fn command(shell: Option<&str>) -> Result<String, AppError> {
    let payload = Base64::encode_string(loader(shell)?.as_bytes());
    // shell 路径的字符白名单已由调用方检查；外层只保留一对单引号，避免 csh 历史展开。
    let fallback = shell.unwrap_or("\"${SHELL:-/bin/sh}\"");
    // base64 不含 printf 格式符，直接作为格式串，外层也无需白名单之外的百分号。
    let body = format!(
        "d=$(mktemp -d 2>/dev/null) && {{ printf {payload} | base64 -d 2>/dev/null >\"$d/pl\" && . \"$d/pl\"; {CLEANUP}; }}; exec {fallback} -l"
    );
    Ok(format!("exec /bin/sh -c {}", sh_quote(&body)?))
}

#[cfg(all(test, unix))]
mod tests;
