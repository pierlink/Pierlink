//! 命令片段的 Tauri 命令：列表、保存、删除。存储在 `pierlink_core::storage`。
//! 变量替换、换行统一、发到终端都在前端（见 src/features/snippets/）。

use crate::data::DbState;
use pierlink_core::error::AppError;
use pierlink_core::storage::Snippet;
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Manager};

/// 片段标题、正文的长度上限（字符）
const TITLE_MAX: usize = 100;
const BODY_MAX: usize = 20_000;

/// 给前端的命令片段；group_name 为空时所有服务器通用
#[derive(Serialize, Deserialize, Type, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SnippetDto {
    pub id: String,
    pub title: String,
    pub body: String,
    pub group_name: Option<String>,
    pub sort_order: i32,
}

/// 保存片段的输入；id 为空时新建
#[derive(Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SnippetInput {
    pub id: Option<String>,
    pub title: String,
    pub body: String,
    pub group_name: Option<String>,
    pub sort_order: i32,
}

impl From<Snippet> for SnippetDto {
    fn from(s: Snippet) -> Self {
        Self {
            id: s.id,
            title: s.title,
            body: s.body,
            group_name: s.group_name,
            sort_order: s.sort_order.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        }
    }
}

/// 校验并整理输入：标题去掉首尾空白且不能为空，分组名为空白时视为通用
fn build(input: SnippetInput) -> Result<Snippet, AppError> {
    let title = input.title.trim().to_owned();
    if title.is_empty() || title.chars().count() > TITLE_MAX || title.contains(['\n', '\r', '\0']) {
        return Err(AppError::new("snippet.invalid_field").with("field", "title"));
    }
    if input.body.trim().is_empty()
        || input.body.chars().count() > BODY_MAX
        || input.body.contains('\0')
    {
        return Err(AppError::new("snippet.invalid_field").with("field", "body"));
    }
    let group_name = input
        .group_name
        .map(|g| g.trim().to_owned())
        .filter(|g| !g.is_empty());
    Ok(Snippet {
        id: input
            .id
            .filter(|x| !x.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        title,
        body: input.body,
        group_name,
        sort_order: input.sort_order.into(),
    })
}

/// 列出全部命令片段（按分组、排序号）
#[tauri::command]
#[specta::specta]
pub async fn snippet_list(app: AppHandle) -> Result<Vec<SnippetDto>, AppError> {
    let list = app.state::<DbState>().with(|db| db.list_snippets())?;
    Ok(list.into_iter().map(SnippetDto::from).collect())
}

/// 新建或修改命令片段；返回保存后的记录
#[tauri::command]
#[specta::specta]
pub async fn snippet_save(app: AppHandle, input: SnippetInput) -> Result<SnippetDto, AppError> {
    let snippet = build(input)?;
    app.state::<DbState>()
        .with(|db| db.upsert_snippet(&snippet))?;
    eprintln!(
        "保存命令片段: id={}, 标题长度={}",
        snippet.id,
        snippet.title.chars().count()
    );
    Ok(snippet.into())
}

/// 删除命令片段
#[tauri::command]
#[specta::specta]
pub async fn snippet_delete(app: AppHandle, id: String) -> Result<(), AppError> {
    app.state::<DbState>().with(|db| db.delete_snippet(&id))?;
    eprintln!("删除命令片段: id={id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(title: &str, body: &str, group: Option<&str>) -> SnippetInput {
        SnippetInput {
            id: None,
            title: title.into(),
            body: body.into(),
            group_name: group.map(Into::into),
            sort_order: 0,
        }
    }

    #[test]
    fn 片段输入校验() {
        let s = build(input(" 拉取 ", "git pull\r\n", Some("  "))).unwrap();
        assert_eq!(s.title, "拉取");
        assert_eq!(s.body, "git pull\r\n", "正文按原样保存，换行在发送时统一");
        assert_eq!(s.group_name, None);
        assert!(!s.id.is_empty());
        assert!(build(input("", "ls", None)).is_err());
        assert!(build(input("a\nb", "ls", None)).is_err());
        assert!(build(input("a", "  ", None)).is_err());
        assert!(build(input("a", "x\0", None)).is_err());
        assert!(build(input(&"长".repeat(101), "ls", None)).is_err());
    }
}
