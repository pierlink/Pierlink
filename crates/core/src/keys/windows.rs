//! 先直接检查 Windows ACL；只有权限不符合要求时才隐藏启动系统工具修复。
use crate::error::AppError;
use std::{
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::Path,
    process::Command,
    sync::OnceLock,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, GetFileSecurityW,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
    GetSidIdentifierAuthority, GetSidSubAuthority, GetSidSubAuthorityCount, IsValidSid,
    OWNER_SECURITY_INFORMATION, PSID, SE_DACL_PROTECTED,
};

fn failed(reason: &str) -> AppError {
    AppError::new("keys.local_failed").with("reason", reason)
}
fn system_command(name: &str) -> Result<Command, AppError> {
    let root = std::env::var_os("SystemRoot").ok_or_else(|| failed("无法获取 Windows 系统目录"))?;
    let root = Path::new(&root);
    if !root.is_absolute() {
        return Err(failed("Windows 系统目录不是绝对路径"));
    }
    let mut command = Command::new(root.join("System32").join(name));
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW：不闪控制台窗口。
    Ok(command)
}
fn current_sid() -> Result<&'static str, AppError> {
    static SID: OnceLock<String> = OnceLock::new();
    if let Some(sid) = SID.get() {
        return Ok(sid);
    }
    let output = system_command("whoami.exe")?
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .map_err(|_| failed("无法获取当前用户 SID"))?;
    if !output.status.success() {
        return Err(failed("获取当前用户 SID 失败"));
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let sid = output
        .rsplit(',')
        .next()
        .map(|s| s.trim().trim_matches('"'))
        .filter(|s| {
            s.starts_with("S-1-") && s.bytes().all(|c| c.is_ascii_digit() || b"S-".contains(&c))
        })
        .ok_or_else(|| failed("无法解析当前用户 SID"))?;
    // 只缓存成功结果，暂时性失败不能阻止后续重试。
    let _ = SID.set(sid.into());
    SID.get()
        .map(String::as_str)
        .ok_or_else(|| failed("无法缓存当前用户 SID"))
}
// 参数只来自 Windows 已验证的安全描述符；复制为文本后不保留内部指针。
fn sid_string(sid: PSID) -> Result<String, AppError> {
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(failed("ACL 中的用户 SID 无效"));
    }
    let authority = unsafe { &*GetSidIdentifierAuthority(sid) }
        .Value
        .iter()
        .fold(0u64, |n, byte| (n << 8) | u64::from(*byte));
    let mut value = format!("S-1-{authority}");
    for i in 0..u32::from(unsafe { *GetSidSubAuthorityCount(sid) }) {
        value.push_str(&format!("-{}", unsafe { *GetSidSubAuthority(sid, i) }));
    }
    Ok(value)
}
fn restricted(path: &Path, directory: bool, sid: &str) -> Result<bool, AppError> {
    let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let info = DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION;
    let mut size = 0;
    unsafe { GetFileSecurityW(path.as_ptr(), info, std::ptr::null_mut(), 0, &mut size) };
    if size == 0 {
        return Err(failed("无法读取密钥 ACL 大小"));
    }
    // 安全描述符和 ACE 需要对齐，不能用 Vec<u8> 强转后直接解引用。
    let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
    let descriptor = buffer.as_mut_ptr().cast();
    if unsafe { GetFileSecurityW(path.as_ptr(), info, descriptor, size, &mut size) } == 0 {
        return Err(failed("无法读取密钥 ACL"));
    }
    let (mut owner, mut defaulted) = (std::ptr::null_mut(), 0);
    if unsafe { GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) } == 0 {
        return Err(failed("无法读取密钥属主"));
    }
    // 提权账户新建文件的默认属主可能是 Administrators；OpenSSH 也认可该属主。
    if ![sid, "S-1-5-32-544", "S-1-5-18"].contains(&sid_string(owner)?.as_str()) {
        return Err(failed("密钥路径不属于当前用户"));
    }
    let (mut present, mut acl, mut control, mut revision) = (0, std::ptr::null_mut::<ACL>(), 0, 0);
    if unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) } == 0
        || unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
    {
        return Err(failed("无法检查密钥 ACL"));
    }
    if present == 0 || acl.is_null() || control & SE_DACL_PROTECTED == 0 {
        return Ok(false);
    }
    // 受限 ACL 只有当前用户的一条完全控制许可，不含继承或其他用户授权。
    if unsafe { (*acl).AceCount } != 1 {
        return Ok(false);
    }
    let mut entry = std::ptr::null_mut();
    if unsafe { GetAce(acl, 0, &mut entry) } == 0 || entry.is_null() {
        return Err(failed("无法读取密钥 ACL 条目"));
    }
    let header = unsafe { &*entry.cast::<ACE_HEADER>() };
    let flags = if directory { 3 } else { 0 }; // 目录允许子文件和子目录继承本人权限。
    if header.AceType != 0 || header.AceFlags != flags {
        // ACCESS_ALLOWED_ACE_TYPE。
        return Ok(false);
    }
    let ace = unsafe { &*entry.cast::<ACCESS_ALLOWED_ACE>() };
    let grantee = std::ptr::addr_of!(ace.SidStart).cast_mut().cast();
    Ok(ace.Mask & 0x001f01ff == 0x001f01ff && sid_string(grantee)? == sid)
}

/// 检查 path 的用户 ACL；directory 决定子项继承方式，必要时修复，失败返回脱敏错误。
pub(super) fn permissions(path: &Path, directory: bool) -> Result<(), AppError> {
    let sid = current_sid()?;
    if restricted(path, directory, sid)? {
        return Ok(());
    }
    let grant = format!("*{sid}:{}F", if directory { "(OI)(CI)" } else { "" });
    for args in [
        vec!["/reset"],
        vec!["/inheritance:r", "/grant:r", grant.as_str()],
    ] {
        if !system_command("icacls.exe")?
            .arg(path)
            .args(args)
            .output()
            .map_err(|_| failed("无法启动密钥 ACL 修复命令"))?
            .status
            .success()
        {
            return Err(failed("无法限制密钥 ACL"));
        }
    }
    if !restricted(path, directory, sid)? {
        return Err(failed("密钥 ACL 修复后仍未限制为本人访问"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s19_检查并修复被放宽的_acl() {
        let root = crate::test_support::tempdir();
        let dir = root.path().join("keys");
        std::fs::create_dir(&dir).unwrap();
        for (path, directory) in [(&dir, true), (&dir.join("private"), false)] {
            if !directory {
                std::fs::write(path, "测试").unwrap();
            }
            permissions(path, directory).unwrap();
            assert!(restricted(path, directory, current_sid().unwrap()).unwrap());
            assert!(
                system_command("icacls.exe")
                    .unwrap()
                    .arg(path)
                    .args(["/grant", "*S-1-1-0:F"])
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            assert!(!restricted(path, directory, current_sid().unwrap()).unwrap());
            permissions(path, directory).unwrap();
            assert!(restricted(path, directory, current_sid().unwrap()).unwrap());
        }
    }
}
