//! 安装完整桌面前的只读检查：管理员权限、磁盘空间、内存和 CPU、要装的软件包大小。
//! 安装本身由用户在终端里运行（要密码时用户自己输），这里只给出判断依据。
use super::{
    desktop::{error, run},
    detection::apt_packages,
    model::*,
};
use crate::{
    error::AppError,
    process::SshProgram,
    ssh::{args::SshCommand, quote::sh_quote},
};
use serde::Deserialize;

/// 装完后每个要写入的文件系统至少还要剩的空间（KiB）
pub const INSTALL_MARGIN_KIB: u64 = 1024 * 1024;
/// 拿不到软件源里的大小时（软件源索引没更新、找不到包、dnf）按这个估计：装好后占用 1 GiB（KiB）
pub const FALLBACK_INSTALLED_KIB: u64 = 1024 * 1024;
/// 同上，要下载 512 MiB（KiB）
pub const FALLBACK_DOWNLOAD_KIB: u64 = 512 * 1024;
/// 内存总量低于 1.8 GiB 提醒配置低（标称 2G 的云主机实际报 1.9G 左右，不提醒）
pub const LOW_MEMORY_KIB: u64 = 18 * 1024 * 1024 / 10;
/// 可用内存低于 700 MiB 提醒配置低
pub const LOW_AVAILABLE_KIB: u64 = 700 * 1024;
/// 少于 2 个 CPU 核提醒配置低
pub const MIN_CPUS: u32 = 2;

#[derive(Deserialize)]
pub(super) struct RawCheck {
    admin: AdminAccess,
    memory: String,
    available_memory: String,
    cpus: String,
    install: RawDisk,
    cache: RawDisk,
    estimate: Option<RawEstimate>,
}

#[derive(Deserialize)]
struct RawDisk {
    available: String,
    mount: String,
}

#[derive(Deserialize)]
struct RawEstimate {
    packages: String,
    records: String,
    installed: String,
    download: String,
}

fn number(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// 用 program/cmd 在服务器上检查能不能装完整桌面：管理员权限、磁盘、内存和 CPU；apt 时模拟安装估算大小。
/// 只读，不安装也不写文件。
///
/// :param distro: 检测到的发行版（决定 apt 要装的包）
/// :param pm: 检测到的包管理器
/// :return: 检查结果；没有认识的包管理器时返回 vnc.install_unsupported
pub fn install_check(
    program: &SshProgram,
    cmd: &SshCommand,
    distro: &Distro,
    pm: PackageManager,
) -> Result<InstallCheck, AppError> {
    let (manager, packages, cache) = match pm {
        PackageManager::Apt => (
            "apt",
            apt_packages(distro).join(" "),
            "/var/cache/apt/archives",
        ),
        PackageManager::Dnf => ("dnf", String::new(), "/var/cache/dnf"),
        PackageManager::None => return Err(error("install_unsupported")),
    };
    let raw: RawCheck = serde_json::from_value(run(
        program,
        cmd,
        &script(manager, &packages, "/usr", cache)?,
        120,
    )?)
    .map_err(|_| error("invalid_report"))?;
    evaluate(raw)
}

/// 检查脚本：manager 为 apt 或 dnf，packages 是空格分隔的包名，后两个是装进系统的位置和下载缓存的位置。
pub(super) fn script(
    manager: &str,
    packages: &str,
    install_path: &str,
    cache_path: &str,
) -> Result<String, AppError> {
    Ok(format!(
        "manager={}\npackages={}\ninstall_path={}\ncache_path={}\n{}",
        sh_quote(manager)?,
        sh_quote(packages)?,
        sh_quote(install_path)?,
        sh_quote(cache_path)?,
        include_str!("install_check.sh")
    ))
}

/// 按检查到的数值算出结论：装好后的文件写到 install 所在的文件系统，下载的包写到 cache 所在的，
/// 同一个文件系统时两者相加；每个文件系统再加 1 GiB 余量。拿不到估算时用固定的保守值。
pub(super) fn evaluate(raw: RawCheck) -> Result<InstallCheck, AppError> {
    let estimate = raw.estimate.and_then(|e| {
        let (packages, records) = (number(&e.packages)?, number(&e.records)?);
        // 软件源里查不全时不用：少算了会把空间判宽
        (records >= packages).then_some((number(&e.installed)?, number(&e.download)?))
    });
    let estimated = estimate.is_none();
    let (installed_kib, download_bytes) =
        estimate.unwrap_or((FALLBACK_INSTALLED_KIB, FALLBACK_DOWNLOAD_KIB * 1024));
    let mut disks: Vec<DiskSpace> = Vec::new();
    for (disk, need) in [
        (&raw.install, installed_kib),
        (&raw.cache, download_bytes.div_ceil(1024)),
    ] {
        let available = number(&disk.available)
            .filter(|_| !disk.mount.is_empty())
            .ok_or_else(|| error("invalid_report"))?;
        match disks.iter_mut().find(|d| d.mount == disk.mount) {
            Some(same) => same.need_kib += need,
            None => disks.push(DiskSpace {
                mount: disk.mount.clone(),
                available_kib: available,
                need_kib: need + INSTALL_MARGIN_KIB,
            }),
        }
    }
    let memory_kib = number(&raw.memory).unwrap_or(0);
    let available_memory_kib = number(&raw.available_memory).unwrap_or(0);
    let cpus = number(&raw.cpus)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    // 读不到（为 0）的不算低
    let low_spec = (memory_kib > 0 && memory_kib < LOW_MEMORY_KIB)
        || (available_memory_kib > 0 && available_memory_kib < LOW_AVAILABLE_KIB)
        || (cpus > 0 && cpus < MIN_CPUS);
    Ok(InstallCheck {
        admin: raw.admin,
        memory_kib,
        available_memory_kib,
        cpus,
        installed_kib,
        download_bytes,
        estimated,
        disks,
        low_spec,
    })
}
