//! 编辑器检测与目录打开；Windows 的命令包装器解析到原生程序，避免 cmd 二次解释路径。
use crate::error::AppError;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct Editor {
    pub id: String,
    pub name: String,
    pub program: PathBuf,
}
/// 可注入的搜索目录；PATH 须预先按操作系统的分隔符拆分。
#[derive(Default)]
pub struct EditorEnvironment {
    pub windows: bool,
    pub path: Vec<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    pub program_files: Vec<PathBuf>,
}
const EDITORS: &[(&str, &str, &str, &str)] = &[
    (
        "code",
        "Visual Studio Code",
        "Microsoft VS Code",
        "Code.exe",
    ),
    (
        "code-insiders",
        "Visual Studio Code Insiders",
        "Microsoft VS Code Insiders",
        "Code - Insiders.exe",
    ),
    ("cursor", "Cursor", "cursor", "Cursor.exe"),
    ("windsurf", "Windsurf", "Windsurf", "Windsurf.exe"),
];
fn native_wrapper(path: &Path, exe: &str) -> Option<PathBuf> {
    let parent = path.parent()?;
    [parent.join(exe), parent.parent()?.join(exe)]
        .into_iter()
        .find(|p| p.is_file())
}
/// 按 env 中的 PATH 和常见 Windows 安装目录查找，返回每种编辑器的第一个原生程序。
/// 空 PATH 项不作为当前目录；仅有 .cmd 而无配套 exe 的包装器不执行。
pub fn detect_with_environment(env: &EditorEnvironment) -> Vec<Editor> {
    let mut found = Vec::new();
    for &(id, name, directory, exe) in EDITORS {
        let mut candidates = Vec::new();
        for dir in env.path.iter().filter(|p| p.is_absolute()) {
            if env.windows {
                candidates.push(dir.join(exe));
                let wrapper = dir.join(format!("{id}.cmd"));
                if wrapper.is_file()
                    && let Some(native) = native_wrapper(&wrapper, exe)
                {
                    candidates.push(native);
                }
            } else {
                candidates.push(dir.join(id));
            }
        }
        if env.windows {
            if let Some(local) = &env.local_app_data {
                candidates.push(local.join("Programs").join(directory).join(exe));
            }
            for root in &env.program_files {
                candidates.push(root.join(directory).join(exe));
            }
        }
        if let Some(program) = candidates.into_iter().find(|p| p.is_file()) {
            found.push(Editor {
                id: id.into(),
                name: name.into(),
                program,
            });
        }
    }
    found
}
/// 无参数；使用当前进程环境检测已安装的常见编辑器，没找到返回空列表。
pub fn detect_editors() -> Vec<Editor> {
    detect_with_environment(&EditorEnvironment {
        windows: cfg!(windows),
        path: std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default(),
        local_app_data: std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        program_files: ["ProgramFiles", "ProgramFiles(x86)"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from)
            .collect(),
    })
}
fn command(editor: &Editor, directory: &Path) -> Result<Command, AppError> {
    if !editor.program.is_absolute() || !editor.program.is_file() {
        return Err(AppError::new("editor.not_found").with("id", &editor.id));
    }
    let extension = editor
        .program
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
        return Err(AppError::new("editor.unsupported_launcher").with_reason(
            "detect.select_the_editor_s_native_executable",
            "请选择编辑器的原生可执行文件",
        ));
    }
    let path = crate::fs_local::plain_canonical(directory)
        .map_err(|_| AppError::new("editor.invalid_directory"))?;
    if !path.is_dir() {
        return Err(AppError::new("editor.invalid_directory"));
    }
    let mut command = Command::new(&editor.program);
    command
        .arg("--new-window")
        .arg("--")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}
/// 用指定 editor 打开本机 directory，返回启动结果；参数独立传递，子进程退出后自动回收。
/// 调用方应从检测结果选择编辑器；只接收已存在的目录。
pub fn open_directory(editor: &Editor, directory: &Path) -> Result<(), AppError> {
    let mut command = command(editor, directory)?;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("编辑器进程回收".into())
        .spawn(move || match command.spawn() {
            Ok(mut child) => {
                let _ = tx.send(Ok(()));
                let _ = child.wait();
            }
            Err(_) => {
                let _ = tx.send(Err(AppError::new("editor.start_failed")));
            }
        })
        .map_err(|_| AppError::new("editor.start_failed"))?;
    rx.recv()
        .map_err(|_| AppError::new("editor.start_failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn 注入环境检测各安装位置及命令包装器() {
        let root = crate::test_support::tempdir();
        let local = root.path().join("local");
        let system = root.path().join("programs");
        let mut env = EditorEnvironment {
            windows: true,
            local_app_data: Some(local.clone()),
            program_files: vec![system.clone()],
            ..Default::default()
        };
        assert!(detect_with_environment(&env).is_empty());
        for (i, &(_, _, directory, exe)) in EDITORS.iter().enumerate() {
            let dir = if i % 2 == 0 {
                local.join("Programs").join(directory)
            } else {
                system.join(directory)
            };
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(exe), "").unwrap();
        }
        assert_eq!(detect_with_environment(&env).len(), 4);
        let native = root.path().join("portable");
        fs::create_dir_all(native.join("bin")).unwrap();
        fs::write(native.join("Code.exe"), "").unwrap();
        fs::write(native.join("bin/code.cmd"), "").unwrap();
        env.path.push(native.join("bin"));
        assert_eq!(
            detect_with_environment(&env)[0].program,
            native.join("Code.exe")
        );
        // 当前机器环境本来可能没有编辑器，公共入口只需保证结果与注入入口一致且不崩溃。
        for editor in detect_editors() {
            assert!(editor.program.is_file());
        }
    }
    #[test]
    fn 打开目录拒绝缺失程序与脚本并保留参数边界() {
        let root = crate::test_support::tempdir();
        let dir = root.path().join("中文 & %PATH% ! $() '目录");
        fs::create_dir(&dir).unwrap();
        let mut editor = Editor {
            id: "code".into(),
            name: "编辑器".into(),
            program: root.path().join("missing"),
        };
        assert_eq!(
            open_directory(&editor, &dir).unwrap_err().code,
            "editor.not_found"
        );
        editor.program = root.path().join("code.cmd");
        fs::write(&editor.program, "").unwrap();
        assert_eq!(
            open_directory(&editor, &dir).unwrap_err().code,
            "editor.unsupported_launcher"
        );
        editor.program = root.path().join("Code.exe");
        fs::write(&editor.program, "").unwrap();
        let cmd = command(&editor, &dir).unwrap();
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            vec![
                std::ffi::OsStr::new("--new-window"),
                std::ffi::OsStr::new("--"),
                dir.as_os_str()
            ]
        );
        assert_eq!(
            open_directory(&editor, &root.path().join("missing"))
                .unwrap_err()
                .code,
            "editor.invalid_directory"
        );
        #[cfg(unix)]
        {
            editor.program = "/usr/bin/true".into();
            open_directory(&editor, &dir).unwrap();
        }
    }
}
