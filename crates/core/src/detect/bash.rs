//! Bash 包装脚本：先加载用户配置，最后追加目录钩子；固定四个文件，不积累启动快照。
use super::{LocalShell, ShellKind};
use crate::error::AppError;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

const USER_RC: &str = r#"if [[ -r ~/.bashrc ]]; then
    . ~/.bashrc
fi
"#;
pub(crate) const LOGIN_RC: &str = r#"# --login 会令 Bash 忽略 --rcfile，因此在包装脚本中按登录顺序加载配置。
if [[ -r /etc/profile ]]; then
    . /etc/profile
fi
for __pierlink_profile in ~/.bash_profile ~/.bash_login ~/.profile; do
    if [[ -r "$__pierlink_profile" ]]; then
        . "$__pierlink_profile"
        break
    fi
done
unset __pierlink_profile
"#;
const APPEND_HOOK: &str = r#"
# 用户配置已经加载；保留标量或数组形式的 PROMPT_COMMAND，只追加一次。
if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == 'declare -a '* ]]; then
    __pierlink_found=
    for __pierlink_command in "${PROMPT_COMMAND[@]}"; do
        if [[ "$__pierlink_command" == __pierlink_report_cwd ]]; then
            __pierlink_found=1
        fi
    done
    if [[ -z $__pierlink_found ]]; then
        PROMPT_COMMAND+=(__pierlink_report_cwd)
    fi
    unset __pierlink_found __pierlink_command
else
    case $'\n'${PROMPT_COMMAND-}$'\n' in
        *$'\n__pierlink_report_cwd\n'*) ;;
        *) PROMPT_COMMAND="${PROMPT_COMMAND:+$PROMPT_COMMAND$'\n'}__pierlink_report_cwd" ;;
    esac
fi
"#;
fn error(text_key: &'static str, operation: &str) -> AppError {
    AppError::new("detect.shell_config_failed").with_reason(text_key, operation)
}
fn script(login: bool, git: bool) -> String {
    let startup = if login { LOGIN_RC } else { USER_RC };
    let path = if git {
        "$(cygpath -w \"$PWD\")"
    } else {
        "$PWD"
    };
    let prediction = if git {
        format!(
            "\nif [[ ${{PIERLINK_PREDICT-}} == 1 ]]; then\n{}\nfi\nunset PIERLINK_PREDICT\n",
            crate::shell_integration::bash_hook()
        )
    } else {
        String::new()
    };
    format!(
        "{startup}\n__pierlink_report_cwd() {{\n    printf '\\033]9;9;\"%s\"\\033\\\\' \"{path}\"\n}}\n{APPEND_HOOK}{prediction}"
    )
}
struct PendingFile(PathBuf);
impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MoveFileExW};
        let from: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_REPLACE_EXISTING) } != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
fn write_script(dir: &Path, name: &str, content: &str) -> Result<PathBuf, AppError> {
    fs::create_dir_all(dir).map_err(|_| {
        error(
            "detect.couldn_t_create_the_bash_wrapper_directory",
            "创建 Bash 包装脚本目录失败",
        )
    })?;
    let dir = crate::fs_local::plain_canonical(dir).map_err(|_| {
        error(
            "detect.couldn_t_resolve_the_bash_wrapper_directory",
            "解析 Bash 包装脚本目录失败",
        )
    })?;
    let target = dir.join(name);
    match fs::read(&target) {
        Ok(bytes) if bytes == content.as_bytes() => return Ok(target),
        Ok(_) => (),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(_) => {
            return Err(error(
                "detect.couldn_t_read_the_bash_wrapper",
                "读取 Bash 包装脚本失败",
            ));
        }
    }
    let pending = PendingFile(dir.join(format!(".bash-{}.tmp", uuid::Uuid::new_v4())));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file: File = options.open(&pending.0).map_err(|_| {
        error(
            "detect.couldn_t_create_a_temporary_bash_wrapper",
            "创建 Bash 包装临时文件失败",
        )
    })?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| {
            error(
                "detect.couldn_t_save_the_bash_wrapper",
                "保存 Bash 包装脚本失败",
            )
        })?;
    drop(file);
    replace(&pending.0, &target).map_err(|_| {
        error(
            "detect.couldn_t_replace_the_bash_wrapper_atomically",
            "原子替换 Bash 包装脚本失败",
        )
    })?;
    Ok(target)
}
fn argument_path(path: &Path) -> Result<String, AppError> {
    let path = path.to_str().ok_or_else(|| {
        error(
            "detect.bash_wrapper_path_is_not_valid_utf_8",
            "Bash 包装脚本路径不是 UTF-8",
        )
    })?;
    #[cfg(windows)]
    {
        // Rust 规范化的扩展长度前缀不能直接交给 MSYS，驱动器及 UNC 都转成 Bash 可读取形式。
        let path = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
            format!("//{unc}")
        } else {
            path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
        };
        Ok(path.replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        Ok(path.to_owned())
    }
}
/// Windows 自带的 System32\bash.exe 是 WSL 启动器：里面的 Linux bash 读不到 Windows 路径的 rcfile，不加钩子
fn is_wsl_launcher(program: &Path) -> bool {
    let text = program
        .to_string_lossy()
        .to_ascii_lowercase()
        .replace('/', "\\");
    text.contains("\\system32\\") || text.contains("\\syswow64\\") || text.contains("\\sysnative\\")
}
pub(super) fn prepare(shell: &mut LocalShell, dir: &Path) -> Result<(), AppError> {
    let git = shell.kind == ShellKind::GitBash;
    if !git
        && !(shell.kind == ShellKind::Other
            && shell
                .program
                .file_name()
                .is_some_and(|n| n == "bash" || n == "bash.exe")
            && !is_wsl_launcher(&shell.program))
    {
        return Ok(());
    }
    let login = shell
        .args
        .iter()
        .any(|s| matches!(s.as_str(), "--login" | "-l" | "-il" | "-li"));
    let name = match (git, login) {
        (false, false) => "bash-cwd.rc",
        (false, true) => "bash-login-cwd.rc",
        (true, false) => "git-bash-cwd.rc",
        (true, true) => "git-bash-login-cwd.rc",
    };
    let path = write_script(dir, name, &script(login, git))?;
    // 所有可能失败的步骤都在修改参数之前完成，失败时 shell 保持原样
    let rcfile = argument_path(&path)?;
    shell
        .args
        .retain(|s| !matches!(s.as_str(), "--login" | "-l" | "-il" | "-li"));
    shell.args.splice(0..0, ["--rcfile".into(), rcfile]);
    if !shell.args.iter().any(|s| s == "-i") {
        shell.args.push("-i".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "bash_tests.rs"]
mod tests;
