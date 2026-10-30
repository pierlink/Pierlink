//! 同步任务建立前检查归属；只读取本实例配置和目标目录标记。
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryCheck {
    pub canonical_path: String,
    pub already_managed: bool,
    pub has_ignore_file: bool,
    pub marker_present: bool,
    pub marker_folder_id: Option<String>,
}
/// 检查 instance_id 是否已管理 target；返回标记及文件夹 ID，由界面对非 pl- 标记默认禁止并允许用户确认。
pub fn check_sync_directory(
    program: &SshProgram,
    cmd: &SshCommand,
    instance_id: &str,
    target: &str,
) -> Result<DirectoryCheck, AppError> {
    if !model::id(instance_id) || !model::path(target) {
        return Err(err("invalid_input"));
    }
    let v = run(
        program,
        cmd,
        &format!(
            r#"
target={}
[ -d "$target" ] || fail not_found
canonical=$(readlink -f "$target")
printf '{{"path":'; json "$canonical"
printf ',"marker":'; if [ -e "$canonical/.stfolder" ]; then printf true; else printf false; fi
marker_id=
for marker in "$canonical"/.stfolder/syncthing-folder-*.txt; do
    [ -f "$marker" ] || continue
    name=${{marker##*/}}; marker_id=${{name#syncthing-folder-}}; marker_id=${{marker_id%.txt}}
    break
done
printf ',"marker_id":'; json "$marker_id"
printf ',"ignore":' ; if [ -e "$canonical/.stignore" ]; then printf true; else printf false; fi
printf ',"xml":'; json "$(cat "$state"/{}/home/config.xml)"; printf '}}'
"#,
            quote(target)?,
            quote(instance_id)?
        ),
        false,
    )?;
    let canonical = v["path"]
        .as_str()
        .filter(|s| model::path(s))
        .ok_or_else(|| err("invalid_report"))?;
    let xml = v["xml"].as_str().ok_or_else(|| err("invalid_report"))?;
    let managed = folder_paths(xml)?.iter().any(|p| p == canonical);
    Ok(DirectoryCheck {
        canonical_path: canonical.into(),
        marker_present: v["marker"].as_bool().ok_or_else(|| err("invalid_report"))?,
        marker_folder_id: v["marker_id"]
            .as_str()
            .filter(|s| model::text(s, 256))
            .map(str::to_owned),
        already_managed: managed,
        has_ignore_file: v["ignore"].as_bool().ok_or_else(|| err("invalid_report"))?,
    })
}
fn folder_paths(xml: &str) -> Result<Vec<String>, AppError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0usize;
    let mut result = vec![];
    loop {
        use quick_xml::events::Event;
        match reader.read_event().map_err(|_| err("invalid_config"))? {
            Event::Start(e) => {
                if depth == 1 && e.name().as_ref() == "folder" {
                    for a in e.attributes() {
                        let a = a.map_err(|_| err("invalid_config"))?;
                        if a.key.as_ref() == "path" {
                            result.push(
                                a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                    .map_err(|_| err("invalid_config"))?
                                    .into_owned(),
                            );
                        }
                    }
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(result)
}
