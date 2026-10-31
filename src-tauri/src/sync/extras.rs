//! 同步页的两个辅助功能：用编辑器打开同步目录、冲突“对比两个版本”。
use super::{DbState, load_task, now_ms};
use pierlink_core::detect::editors::{self, Editor};
use pierlink_core::error::AppError;
use pierlink_core::sync::{self, ConflictDiff, DiffFile, DiffKind};
use serde::Serialize;
use specta::Type;
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

/// 检测到的编辑器
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EditorDto {
    pub id: String,
    pub name: String,
}

/// 这台电脑上的编辑器和这个任务上次用的
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct EditorsDto {
    pub editors: Vec<EditorDto>,
    /// 默认用哪个：任务上次用过且还在的，否则 VS Code，否则第一个；一个都没有时为 null
    pub preferred: Option<String>,
}

/// 冲突对比里一边的文件
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiffFileDto {
    pub size: f64,
    pub modified_ms: f64,
    /// 文本的编码（UTF-8、UTF-16 等）；二进制为 null
    pub encoding: Option<String>,
}

/// 并排对比的一行
#[derive(Serialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiffLineDto {
    pub left_number: Option<u32>,
    pub right_number: Option<u32>,
    pub left: Option<String>,
    pub right: Option<String>,
    /// equal / deleted / added / modified
    pub kind: String,
}

/// 冲突对比结果：left 是原文件（当前版本），right 是冲突副本
#[derive(Serialize, Type, Clone)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConflictDiffDto {
    Text {
        left: DiffFileDto,
        right: DiffFileDto,
        lines: Vec<DiffLineDto>,
    },
    Binary {
        left: DiffFileDto,
        right: DiffFileDto,
    },
    #[serde(rename_all = "camelCase")]
    TooLarge {
        left: DiffFileDto,
        right: DiffFileDto,
        limit: f64,
    },
}

fn file_dto(f: DiffFile) -> DiffFileDto {
    DiffFileDto {
        size: f.size as f64,
        modified_ms: f.modified_ms as f64,
        encoding: f.encoding,
    }
}

impl From<ConflictDiff> for ConflictDiffDto {
    fn from(d: ConflictDiff) -> Self {
        match d {
            ConflictDiff::Text { left, right, lines } => Self::Text {
                left: file_dto(left),
                right: file_dto(right),
                lines: lines
                    .into_iter()
                    .map(|l| DiffLineDto {
                        left_number: l.left_number,
                        right_number: l.right_number,
                        left: l.left,
                        right: l.right,
                        kind: match l.kind {
                            DiffKind::Equal => "equal",
                            DiffKind::Deleted => "deleted",
                            DiffKind::Added => "added",
                            DiffKind::Modified => "modified",
                        }
                        .into(),
                    })
                    .collect(),
            },
            ConflictDiff::Binary { left, right } => Self::Binary {
                left: file_dto(left),
                right: file_dto(right),
            },
            ConflictDiff::TooLarge { left, right, limit } => Self::TooLarge {
                left: file_dto(left),
                right: file_dto(right),
                limit: limit as f64,
            },
        }
    }
}

fn task_failed(e: impl ToString) -> AppError {
    AppError::new("sync.task_failed").with("reason", e.to_string())
}

/// 默认的编辑器：任务记住的 → VS Code → 第一个
fn preferred<'a>(list: &'a [Editor], remembered: Option<&str>) -> Option<&'a Editor> {
    remembered
        .and_then(|id| list.iter().find(|e| e.id == id))
        .or_else(|| list.iter().find(|e| e.id == "vscode"))
        .or_else(|| list.first())
}

/// 列出这台电脑上的编辑器（VS Code、Cursor 等），以及这个任务默认用哪个
///
/// :param task_id: 任务 ID
/// :return: 编辑器列表和默认项
#[tauri::command]
#[specta::specta]
pub async fn sync_editors(app: AppHandle, task_id: String) -> Result<EditorsDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (task, _) = load_task(&app, &task_id)?;
        let list = editors::detect_editors();
        let preferred = preferred(&list, task.editor_id.as_deref()).map(|e| e.id.clone());
        Ok(EditorsDto {
            editors: list
                .into_iter()
                .map(|e| EditorDto {
                    id: e.id,
                    name: e.name,
                })
                .collect(),
            preferred,
        })
    })
    .await
    .map_err(task_failed)?
}

/// 用编辑器打开任务的本机目录；指定了编辑器时记住它，下次默认用它
///
/// :param task_id: 任务 ID
/// :param editor_id: 编辑器 ID；null 用默认的
/// :return: 找不到编辑器返回 editor.not_found
#[tauri::command]
#[specta::specta]
pub async fn sync_open_in_editor(
    app: AppHandle,
    task_id: String,
    editor_id: Option<String>,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (mut task, _) = load_task(&app, &task_id)?;
        let list = editors::detect_editors();
        let editor = match editor_id.as_deref() {
            Some(id) => list.iter().find(|e| e.id == id),
            None => preferred(&list, task.editor_id.as_deref()),
        }
        .ok_or_else(|| AppError::new("editor.not_found"))?;
        editors::open_directory(editor, &PathBuf::from(&task.local_path))?;
        eprintln!(
            "用编辑器打开同步目录: 任务={}, 编辑器={}",
            task.name, editor.name
        );
        if editor_id.is_some() && task.editor_id.as_deref() != Some(editor.id.as_str()) {
            task.editor_id = Some(editor.id.clone());
            task.updated_at = now_ms();
            app.state::<DbState>()
                .with(|db| db.update_sync_task(&task))?;
        }
        Ok(())
    })
    .await
    .map_err(task_failed)?
}

/// 对比一个冲突的两个版本（原文件和冲突副本）；只读本机这一侧
///
/// :param task_id: 任务 ID
/// :param path: 原文件的相对路径
/// :param conflict_file: 冲突副本的相对路径
/// :return: 文本并排对比、二进制只给大小和时间、太大时给大小
#[tauri::command]
#[specta::specta]
pub async fn sync_conflict_diff(
    app: AppHandle,
    task_id: String,
    path: String,
    conflict_file: String,
) -> Result<ConflictDiffDto, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (task, _) = load_task(&app, &task_id)?;
        let root = PathBuf::from(&task.local_path);
        let rules: Vec<String> = task.ignore_rules.lines().map(str::to_owned).collect();
        // 从当前的冲突列表里找，不直接信界面传来的路径
        let conflict = sync::list_conflicts(&root, &rules, 200)?
            .conflicts
            .into_iter()
            .find(|c| c.path == path && c.conflict_file == conflict_file)
            .ok_or_else(|| AppError::new("sync.conflict_changed"))?;
        sync::compare_conflict(&root, &conflict).map(Into::into)
    })
    .await
    .map_err(task_failed)?
}
