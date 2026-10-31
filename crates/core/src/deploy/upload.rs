use super::*;
use std::{
    borrow::Cow,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

/// 根据 cmd 选择批量上传连接，返回借用或调整后的命令；引擎不匹配时返回错误。
pub(crate) fn upload_command(cmd: &SshCommand) -> Result<Cow<'_, SshCommand>, AppError> {
    use crate::{
        builtin::ConnectionClass,
        ssh::engine::{Engine, Feature, engine_for_feature},
    };
    let Some(context) = &cmd.builtin else {
        return Ok(Cow::Borrowed(cmd));
    };
    if engine_for_feature(&context.server, Feature::DeployUpload).engine != Engine::Builtin {
        return Err(AppError::new("ssh.invalid_argument")
            .with_reason(
                "deploy.deployment_upload_requires_a_background_command_for_the_selected_engine",
                "部署上传需要与所选引擎匹配的后台命令",
            )
            .with_text(
                "field",
                "deploy.deployment_upload_requires_a_background_command_for_the_selected_engine",
                "部署上传需要与所选引擎匹配的后台命令",
            ));
    }
    let mut command = cmd.clone();
    let mut context = (**context).clone();
    context.class = ConnectionClass::Bulk;
    command.builtin = Some(std::sync::Arc::new(context));
    Ok(Cow::Owned(command))
}

pub(super) fn base64(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let a = c[0];
        let b = c.get(1).copied().unwrap_or(0);
        let d = c.get(2).copied().unwrap_or(0);
        out.push(A[(a >> 2) as usize] as char);
        out.push(A[((a & 3) << 4 | b >> 4) as usize] as char);
        out.push(if c.len() > 1 {
            A[((b & 15) << 2 | d >> 6) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            A[(d & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
/// 经 program/cmd 分块上传 local_gz，校验解压后的 expected_sha256；progress 报告压缩字节，cancel 在块之间检查。
#[allow(clippy::too_many_arguments)]
pub fn upload_binary(
    program: &SshProgram,
    cmd: &SshCommand,
    local_gz: &Path,
    expected_sha256: &str,
    version: &str,
    arch: Arch,
    progress: &dyn Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<String, AppError> {
    model::version(version)?;
    let version = version.trim_start_matches('v');
    if expected_sha256.len() != 64 || !expected_sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(err("invalid_input"));
    }
    let command = upload_command(cmd)?;
    let execute = |body: &str| run(program, command.as_ref(), body, true);
    let destination = format!(
        "\"$binroot\"/{}/{}/syncthing",
        quote(version)?,
        quote(arch.directory())?
    );
    upload_binary_to(
        local_gz,
        expected_sha256,
        &destination,
        &execute,
        progress,
        cancel,
    )
}

/// 将 local_gz 分块传到已转义的 destination，经 execute 执行并核对解压后的 expected_sha256。
/// progress 报告压缩字节，cancel 在块之间取消；成功返回服务器上的程序路径。
pub(crate) fn upload_binary_to(
    local_gz: &Path,
    expected_sha256: &str,
    destination: &str,
    execute: &dyn Fn(&str) -> Result<serde_json::Value, AppError>,
    progress: &dyn Fn(u64, u64),
    cancel: &AtomicBool,
) -> Result<String, AppError> {
    let mut file = File::open(local_gz).map_err(|_| err("local_io"))?;
    let total = file.metadata().map_err(|_| err("local_io"))?.len();
    if total == 0 || total > 512 * 1024 * 1024 {
        return Err(err("invalid_input"));
    }
    let setup = format!(
        r#"
command -v base64 >/dev/null || fail missing_base64
command -v gzip >/dev/null || fail missing_gzip
file={destination}
safe_path "$file"
if [ ! -d "${{file%/*}}" ]; then
    mkdir -p -- "${{file%/*}}"
    printf true > "${{file%/*}}/.pierlink-created"
fi
# 断点片段最多保留七天；只清理本应用创建的程序目录。
for marker in "$binroot"/*/*/.pierlink-created "$binroot"/*/.pierlink-created; do
    [ -f "$marker" ] || continue
    candidate=${{marker%/.pierlink-created}}
    safe_path "$candidate"
    find "$candidate" -name '*.gz.part' -type f -mtime +6 -exec rm -f -- {{}} \;
done
part=$file.gz.part
safe_path "$part"
expected={}
if [ -f "$file" ] && [ "$(sha256_of < "$file")" = "$expected" ]; then printf '{{"complete":true,"path":'; json "$file"; printf '}}'; exit 0; fi
size=0
if [ -f "$part" ]; then size=$(wc -c < "$part" | tr -d ' '); fi
if [ "$size" -gt {total} ]; then
    [ -f "${{file%/*}}/.pierlink-created" ] || fail upload_conflict
    rm -f -- "$part"; size=0
fi
"#,
        quote(&expected_sha256.to_ascii_lowercase())?
    );
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(err("cancelled"));
        }
        let value = execute(&format!(
            "{setup}\nprintf '{{\"complete\":false,\"size\":%s}}' \"$size\""
        ))?;
        if value["complete"] == true {
            let p = value["path"]
                .as_str()
                .filter(|p| model::path(p))
                .ok_or_else(|| err("invalid_report"))?;
            progress(total, total);
            return Ok(p.into());
        }
        let size = value["size"]
            .as_u64()
            .filter(|s| *s <= total)
            .ok_or_else(|| err("upload_conflict"))?;
        progress(size, total);
        if cancel.load(Ordering::Acquire) {
            return Err(err("cancelled"));
        }
        if size == total {
            let finish = format!(
                r#"{setup}
[ "$size" = {total} ] || fail upload_conflict
temporary=$file.unpack
safe_path "$temporary"
gzip -dc "$part" > "$temporary" || {{ rm -f -- "$part"; fail checksum_mismatch; }}
if [ "$(sha256_of < "$temporary")" != "$expected" ]; then rm -f -- "$part"; fail checksum_mismatch; fi
chmod 755 "$temporary"
mv -f -- "$temporary" "$file"
temporary=
rm -f -- "$part"
printf '{{"path":'; json "$file"; printf '}}'
"#
            );
            let v = execute(&finish)?;
            let p = v["path"]
                .as_str()
                .filter(|p| model::path(p))
                .ok_or_else(|| err("invalid_report"))?;
            progress(total, total);
            return Ok(p.into());
        }
        file.seek(SeekFrom::Start(size))
            .map_err(|_| err("local_io"))?;
        let mut buf = vec![0; ((total - size).min(1024 * 1024)) as usize];
        file.read_exact(&mut buf).map_err(|_| err("local_io"))?;
        let data = base64(&buf);
        execute(&format!(
            r#"{setup}
[ "$size" = {size} ] || fail upload_conflict
base64 -d >> "$part" <<'PIERLINK_DATA'
{data}
PIERLINK_DATA
printf '{{"ok":true}}'
"#
        ))?;
    }
}
