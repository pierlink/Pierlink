//! 旧版本浏览与恢复；当前文件存在时 Syncthing 会先将当前内容保存为新版本。
use super::{
    LocalSync,
    conflicts::relative,
    invalid_response,
    time::{format_ms, parse_ms},
};
use crate::{error::AppError, syncthing::folder::validate_task_folder_id};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedFile {
    pub path: String,
    pub version_time_ms: i64,
    pub modified_ms: i64,
    pub size: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreError {
    pub path: String,
    pub reason: String,
}
/// 返回 local 上 folder_id 的旧版本；prefix 为可选相对路径前缀，关闭旧版本时返回空列表。
pub fn list_versions(
    local: &LocalSync,
    folder_id: &str,
    prefix: Option<&str>,
) -> Result<Vec<VersionedFile>, AppError> {
    if let Some(p) = prefix.filter(|s| !s.is_empty()) {
        relative(p.trim_end_matches('/'))?;
    }
    let value = local.client().folder_versions(folder_id)?;
    let mut result = Vec::new();
    for (path, versions) in value.as_object().ok_or_else(invalid_response)? {
        relative(path)?;
        if prefix.is_some_and(|p| {
            let p = p.trim_end_matches('/');
            !p.is_empty() && path != p && !path.starts_with(&format!("{p}/"))
        }) {
            continue;
        }
        for v in versions.as_array().ok_or_else(invalid_response)? {
            result.push(VersionedFile {
                path: path.clone(),
                version_time_ms: parse_ms(v["versionTime"].as_str().ok_or_else(invalid_response)?)?,
                modified_ms: parse_ms(v["modTime"].as_str().ok_or_else(invalid_response)?)?,
                size: v["size"].as_u64().ok_or_else(invalid_response)?,
            });
        }
    }
    result.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(b.version_time_ms.cmp(&a.version_time_ms))
    });
    Ok(result)
}
/// 恢复 folder_id 中 picks 指定的相对路径及版本时间，返回逐文件失败原因。
/// 当前文件存在时，Syncthing 先把当前内容保存为新版本，再恢复选择的内容。
pub fn restore_versions(
    local: &LocalSync,
    folder_id: &str,
    picks: &[(String, i64)],
) -> Result<Vec<RestoreError>, AppError> {
    validate_task_folder_id(folder_id)?;
    let mut body = Map::new();
    for (path, time) in picks {
        relative(path)?;
        if body
            .insert(path.clone(), Value::String(format_ms(*time)))
            .is_some()
        {
            return Err(super::error(
                "sync.multiple_versions_of_the_same_file_cannot_be_restored_together",
                "同一文件不能同时恢复多个版本",
            ));
        }
    }
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let value = local.client().call(
        "POST",
        "/rest/folder/versions",
        &[("folder", folder_id)],
        Some(Value::Object(body)),
    )?;
    let mut errors = Vec::new();
    for (path, reason) in value.as_object().ok_or_else(invalid_response)? {
        let reason = reason.as_str().ok_or_else(invalid_response)?;
        if !reason.is_empty() {
            errors.push(RestoreError {
                path: path.clone(),
                reason: local
                    .client()
                    .redact_text(reason)
                    .chars()
                    .take(512)
                    .collect(),
            });
        }
    }
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syncthing::client::Client;
    use serde_json::json;
    #[test]
    fn 版本列表过滤恢复及逐文件失败() {
        let mut client = Client::new("http://127.0.0.1:1", "secret").unwrap();
        client.test_handler = Some(std::sync::Arc::new(|method, _, _, body| {
            if method == "GET" {
                Ok(
                    json!({"a":[{"versionTime":"2026-10-22T00:00:00Z","modTime":"2026-10-21T00:00:00Z","size":3}]}),
                )
            } else {
                assert_eq!(body.unwrap()["a"], "2026-10-22T00:00:00.000Z");
                Ok(json!({"a":"permission denied secret"}))
            }
        }));
        let local = LocalSync {
            address_updates: Default::default(),
            instance: None,
            client,
            id: "test".into(),
        };
        let id = "pl-1234567890123456";
        assert!(list_versions(&local, id, Some("b")).unwrap().is_empty());
        let v = list_versions(&local, id, None).unwrap().remove(0);
        let errors = restore_versions(&local, id, &[(v.path, v.version_time_ms)]).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "a");
        assert_eq!(errors[0].reason, "permission denied ***");
        assert!(restore_versions(&local, id, &[("../x".into(), 0)]).is_err());
        assert!(restore_versions(&local, id, &[]).unwrap().is_empty());
    }

    #[test]
    fn 恢复失败原因先脱敏再按字符截断() {
        let mut client = Client::new("http://127.0.0.1:1", "secret").unwrap();
        client.test_handler = Some(std::sync::Arc::new(|_, _, _, _| {
            Ok(json!({"a": format!("secret{}", "错".repeat(600)), "b": ""}))
        }));
        let local = LocalSync {
            address_updates: Default::default(),
            instance: None,
            client,
            id: "test".into(),
        };
        let errors = restore_versions(&local, "pl-1234567890123456", &[("a".into(), 0)]).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].reason, format!("***{}", "错".repeat(509)));
        assert_eq!(errors[0].reason.chars().count(), 512);
    }
}

#[cfg(test)]
mod revision_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn 目录前缀按边界且冒号版本可浏览恢复() {
        let mut client =
            crate::syncthing::client::Client::new("http://127.0.0.1:1", "secret").unwrap();
        client.test_handler = Some(std::sync::Arc::new(|method, _, _, body| {
            if method == "POST" {
                assert!(body.unwrap().get("src/report:2026").is_some());
                return Ok(json!({}));
            }
            let version = json!([{"versionTime":"2026-10-22T00:00:00Z", "modTime":"2026-10-22T00:00:00Z", "size":1}]);
            Ok(json!({"src/report:2026":version,"srcfoo/x":version}))
        }));
        let local = LocalSync {
            address_updates: Default::default(),
            instance: None,
            client,
            id: "a".into(),
        };
        for prefix in ["src", "src/"] {
            let files = list_versions(&local, "pl-1234567890123456", Some(prefix)).unwrap();
            assert_eq!(files.len(), 1);
            assert!(
                restore_versions(
                    &local,
                    "pl-1234567890123456",
                    &[(files[0].path.clone(), files[0].version_time_ms)]
                )
                .unwrap()
                .is_empty()
            );
        }
    }
}
