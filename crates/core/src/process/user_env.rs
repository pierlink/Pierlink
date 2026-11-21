//! 启动子进程时用的环境变量。
//!
//! Windows 上进程的环境变量是启动那一刻定下的：在系统设置里新加变量、装了新程序改了 PATH，
//! 已经开着的程序看不到。所以每次启动终端、执行命令时从系统重新取一份当前用户的环境变量
//! （和资源管理器新开一个程序拿到的一样），不用重启就对新开的终端生效；
//! 本进程自己多出来的变量（启动它的程序给的）照旧带上。只有 Windows 这样做。
use std::ffi::OsString;

type Vars = Vec<(OsString, OsString)>;

/// 子进程的基础环境变量（名字、值）；取不到系统那一份时退回本进程的环境变量。
#[cfg(windows)]
pub(crate) fn base() -> Vars {
    let Some(fresh) = windows::fresh() else {
        return std::env::vars_os().collect();
    };
    // 只有本进程才有的变量在第一次用时记下：之后系统里删掉的变量不会被当成“本进程多出来的”又带回来
    static EXTRAS: std::sync::OnceLock<Vars> = std::sync::OnceLock::new();
    let extras = EXTRAS.get_or_init(|| only_in_process(std::env::vars_os(), &fresh));
    merge(fresh, extras)
}

/// 环境变量名不分大小写：比较时统一成大写
fn name_key(name: &std::ffi::OsStr) -> String {
    name.to_string_lossy().to_uppercase()
}

/// process 里有、fresh 里没有的变量
fn only_in_process(
    process: impl Iterator<Item = (OsString, OsString)>,
    fresh: &[(OsString, OsString)],
) -> Vars {
    let known: std::collections::HashSet<String> = fresh.iter().map(|(k, _)| name_key(k)).collect();
    process
        .filter(|(name, _)| !known.contains(&name_key(name)))
        .collect()
}

/// fresh 加上 extras 里它没有的变量
fn merge(mut fresh: Vars, extras: &[(OsString, OsString)]) -> Vars {
    let known: std::collections::HashSet<String> = fresh.iter().map(|(k, _)| name_key(k)).collect();
    fresh.extend(
        extras
            .iter()
            .filter(|(name, _)| !known.contains(&name_key(name)))
            .cloned(),
    );
    fresh
}

/// 环境块（一串“名字=值”，各以 0 结尾，最后再多一个 0）拆成名字和值；
/// 名字为空、没有等号的项跳过，等号从第二个字符找起（Windows 有“=C:”这种以等号开头的名字）。
fn split_block(block: &[u16]) -> Vec<(Vec<u16>, Vec<u16>)> {
    block
        .split(|unit| *unit == 0)
        .take_while(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let at = entry
                .iter()
                .skip(1)
                .position(|unit| *unit == u16::from(b'='))?
                + 1;
            Some((entry[..at].to_vec(), entry[at + 1..].to_vec()))
        })
        .collect()
}

#[cfg(windows)]
mod windows {
    use super::{Vars, split_block};
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{TOKEN_DUPLICATE, TOKEN_QUERY},
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    /// 环境块最多读多少个字符（系统的上限是 32767，留足余量；防止块没有正常结尾时一直读下去）
    const MAX_UNITS: usize = 1 << 20;

    /// 从系统取当前用户的环境变量（不带本进程的）；取不到返回 None。
    pub(super) fn fresh() -> Option<Vars> {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY | TOKEN_DUPLICATE,
                &mut token,
            ) == 0
            {
                return None;
            }
            let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
            let created = CreateEnvironmentBlock(&mut block, token, 0);
            CloseHandle(token);
            if created == 0 || block.is_null() {
                return None;
            }
            // 块以两个连着的 0 结尾：先量出长度再拆
            let start = block as *const u16;
            let mut len = 0;
            while len + 1 < MAX_UNITS && (*start.add(len) != 0 || *start.add(len + 1) != 0) {
                len += 1;
            }
            let vars = split_block(std::slice::from_raw_parts(start, len))
                .into_iter()
                .map(|(name, value)| (OsString::from_wide(&name), OsString::from_wide(&value)))
                .collect::<Vars>();
            DestroyEnvironmentBlock(block);
            // 一个变量都没有不正常（至少有 SystemRoot、PATH）：当作没取到
            (!vars.is_empty()).then_some(vars)
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn 取到的环境变量里有系统目录和用户目录() {
            let vars = super::fresh().unwrap();
            for name in ["SYSTEMROOT", "USERPROFILE", "PATH"] {
                assert!(
                    vars.iter().any(|(key, value)| {
                        key.to_string_lossy().to_uppercase() == name && !value.is_empty()
                    }),
                    "{name}"
                );
            }
        }

        #[test]
        fn 本进程多出来的变量照旧带上() {
            // 测试程序启动时就有的变量：CARGO 给的，系统那一份里没有
            let base = super::super::base();
            assert!(
                base.iter()
                    .any(|(key, _)| key.to_string_lossy() == "CARGO_PKG_NAME")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(list: &[(&str, &str)]) -> Vars {
        list.iter().map(|(k, v)| (k.into(), v.into())).collect()
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn 环境块按名字和值拆开() {
        let block =
            wide("=C:=C:\\目录\0Path=C:\\a;C:\\b\0空值=\0A=b=c\0没有等号\0\0后面的不读=1\0");
        let got: Vec<(String, String)> = split_block(&block)
            .into_iter()
            .map(|(k, v)| {
                (
                    String::from_utf16(&k).unwrap(),
                    String::from_utf16(&v).unwrap(),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = [
            ("=C:", "C:\\目录"),
            ("Path", "C:\\a;C:\\b"),
            ("空值", ""),
            ("A", "b=c"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(got, expected);
        assert!(split_block(&[]).is_empty());
    }

    #[test]
    fn 系统那一份优先_本进程多出来的补上_名字不分大小写() {
        let fresh = vars(&[("Path", "新的"), ("NEW_VAR", "1")]);
        let process = vars(&[("PATH", "旧的"), ("FROM_PARENT", "x"), ("=C:", "C:\\")]);
        let extras = only_in_process(process.into_iter(), &fresh);
        assert_eq!(extras, vars(&[("FROM_PARENT", "x"), ("=C:", "C:\\")]));
        // 之后系统里删掉了 NEW_VAR、加了和多出来的同名的变量
        let later = vars(&[("Path", "更新的"), ("from_parent", "系统里的")]);
        assert_eq!(
            merge(later, &extras),
            vars(&[
                ("Path", "更新的"),
                ("from_parent", "系统里的"),
                ("=C:", "C:\\")
            ])
        );
    }
}
