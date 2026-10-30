use super::{
    desktop::{check_binary, error, run, verify_binary},
    model::*,
    parse,
};
use crate::{
    error::AppError,
    process::SshProgram,
    ssh::{args::SshCommand, quote::sh_quote},
};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub(super) const URL: &str = "https://downloads.sourceforge.net/project/tigervnc/stable/1.16.2/tigervnc-1.16.2.x86_64.tar.gz";
pub(super) const SHA256: &str = "5b70c84baefc09a030cfc78315c34ccb55b2a0dde4092b7da67a1962c5f0dea6";
const MAX_PACKAGE: u64 = 64 * 1024 * 1024;
const MAX_BINARY: u64 = 128 * 1024 * 1024;

fn cancelled(cancel: &AtomicBool) -> Result<(), AppError> {
    if cancel.load(Ordering::Acquire) {
        Err(error("cancelled"))
    } else {
        Ok(())
    }
}

fn failed(
    source: AcquireSource,
    e: AppError,
    steps: &mut Vec<AcquireStep>,
    progress: &dyn Fn(AcquireStep),
) {
    let step = AcquireStep::Failed { source, error: e };
    progress(step.clone());
    steps.push(step);
}

fn official_check(d: &VncDetection) -> Result<(), AppError> {
    if d.arch != "x86_64" {
        return Err(error("unsupported_arch").with("arch", &d.arch));
    }
    if !d.official_pkg.ok {
        return Err(error("official_prereq").with("missing", d.official_pkg.missing.join(", ")));
    }
    Ok(())
}

/// 经 program/cmd 按 detection 与 opts 获取可用 TigerVNC；progress 报阶段与字节数，cancel 在阶段和块之间取消。
/// 失败返回 acquire_failed，steps 是按尝试顺序序列化的 AcquireStep::Failed 数组。
pub fn acquire_xvnc(
    program: &SshProgram,
    cmd: &SshCommand,
    detection: &VncDetection,
    opts: AcquireOptions,
    progress: &dyn Fn(AcquireStep),
    cancel: &AtomicBool,
) -> Result<XvncBinary, AppError> {
    let mut steps = Vec::new();
    for source in [
        AcquireSource::Installed,
        AcquireSource::Apt,
        AcquireSource::ServerDownload,
        AcquireSource::LocalDownload,
        AcquireSource::ManualPackage,
    ] {
        cancelled(cancel)?;
        progress(AcquireStep::Started { source });
        let result = match source {
            AcquireSource::Installed => detection
                .tigervnc
                .as_ref()
                .ok_or_else(|| {
                    error("not_tigervnc")
                        .with_reason("vnc.no_installed_tigervnc_found", "未找到已安装的 TigerVNC")
                })
                .and_then(|b| {
                    check_binary(b)?;
                    verify_binary(program, cmd, &b.path)
                }),
            AcquireSource::Apt if detection.package_manager == PackageManager::Apt => {
                remote_package_progress(program, cmd, true, progress)
            }
            AcquireSource::Apt => Err(error("download_failed")
                .with("source", "Apt")
                .with_reason("vnc.server_has_no_apt_get", "服务器没有 apt-get")),
            _ => official_check(detection).and_then(|()| match source {
                AcquireSource::ServerDownload => {
                    remote_package_progress(program, cmd, false, progress)
                }
                AcquireSource::LocalDownload if opts.allow_official_download => {
                    local_package(program, cmd, None, source, progress, cancel)
                }
                AcquireSource::LocalDownload => Err(error("download_failed")
                    .with("source", "LocalDownload")
                    .with_reason(
                        "vnc.downloads_on_this_computer_were_not_permitted",
                        "未允许电脑下载",
                    )),
                AcquireSource::ManualPackage => opts
                    .manual_package
                    .as_deref()
                    .ok_or_else(|| {
                        error("download_failed")
                            .with("source", "ManualPackage")
                            .with_reason(
                                "vnc.no_manually_downloaded_package_was_provided",
                                "未提供手动下载包",
                            )
                    })
                    .and_then(|p| local_package(program, cmd, Some(p), source, progress, cancel)),
                _ => unreachable!(),
            }),
        };
        cancelled(cancel)?;
        match result {
            Ok(binary) => {
                progress(AcquireStep::Ready {
                    binary: binary.clone(),
                });
                return Ok(binary);
            }
            Err(e) if e.code.ends_with(".cancelled") => return Err(error("cancelled")),
            Err(e) => failed(source, e, &mut steps, progress),
        }
    }
    Err(error("acquire_failed").with(
        "steps",
        serde_json::to_string(&steps).map_err(|_| error("invalid_report"))?,
    ))
}

#[cfg(test)]
pub(super) fn remote_package(
    program: &SshProgram,
    cmd: &SshCommand,
    apt: bool,
) -> Result<XvncBinary, AppError> {
    remote_package_progress(program, cmd, apt, &|_| {})
}

fn remote_package_progress(
    program: &SshProgram,
    cmd: &SshCommand,
    apt: bool,
    progress: &dyn Fn(AcquireStep),
) -> Result<XvncBinary, AppError> {
    let body = format!(
        "source={}\nurl={}\nexpected={}\n{}\n{}\n{}",
        if apt { "apt" } else { "official" },
        sh_quote(URL)?,
        sh_quote(SHA256)?,
        include_str!("apt.sh"),
        include_str!("acquire.sh"),
        include_str!("install.sh")
    );
    let value = run(program, cmd, &body, if apt { 900 } else { 180 })?;
    if let Some(bytes) = value["downloaded_bytes"].as_u64() {
        progress(AcquireStep::Download {
            source: if apt {
                AcquireSource::Apt
            } else {
                AcquireSource::ServerDownload
            },
            bytes,
            total: Some(bytes),
        });
    }
    binary_result(value)
}

/// 经 program/cmd 免 sudo 获取 Openbox 与 xterm，返回远程前缀路径。
/// progress 沿用 Apt 下载字节事件，cancel 在远程获取前后检查；已有完整前缀直接复用。
pub fn acquire_light_desktop(
    program: &SshProgram,
    cmd: &SshCommand,
    progress: &dyn Fn(AcquireStep),
    cancel: &AtomicBool,
) -> Result<String, AppError> {
    cancelled(cancel)?;
    progress(AcquireStep::Started {
        source: AcquireSource::Apt,
    });
    let body = format!(
        "light_session={}\nlight_dbus={}\n{}\n{}",
        sh_quote(include_str!("light_session.sh"))?,
        sh_quote(include_str!("light_dbus.sh"))?,
        include_str!("apt.sh"),
        include_str!("light.sh")
    );
    let result = run(program, cmd, &body, 900);
    cancelled(cancel)?;
    let value = result.inspect_err(|e| {
        progress(AcquireStep::Failed {
            source: AcquireSource::Apt,
            error: e.clone(),
        })
    })?;
    let bytes = value["downloaded_bytes"]
        .as_u64()
        .ok_or_else(|| error("invalid_report"))?;
    progress(AcquireStep::Download {
        source: AcquireSource::Apt,
        bytes,
        total: Some(bytes),
    });
    value["prefix"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| error("invalid_report"))
}

fn binary_result(v: serde_json::Value) -> Result<XvncBinary, AppError> {
    let text = v["version"]
        .as_str()
        .ok_or_else(|| error("invalid_report"))?;
    let binary = XvncBinary {
        path: v["path"]
            .as_str()
            .ok_or_else(|| error("invalid_report"))?
            .into(),
        version: parse::version(text).unwrap_or((0, 0, 0)),
        flavor: parse::flavor("", text),
    };
    check_binary(&binary)?;
    Ok(binary)
}

fn download(
    path: &Path,
    source: AcquireSource,
    progress: &dyn Fn(AcquireStep),
    cancel: &AtomicBool,
) -> Result<(), AppError> {
    let fail = |text_key: &'static str, reason: &str| {
        error("download_failed")
            .with_text("source", "vnc.computer", "电脑")
            .with_reason(text_key, reason)
    };
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .https_only(true)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_global(Some(Duration::from_secs(120)))
            .build(),
    );
    let mut response = agent
        .get(URL)
        .call()
        .map_err(|_| fail("vnc.download_unreachable", "连不上下载地址"))?;
    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if total.is_some_and(|n| n > MAX_PACKAGE) {
        return Err(fail("vnc.download_too_large", "文件太大"));
    }
    let mut file = File::create(path).map_err(|_| error("local_io"))?;
    let mut reader = response.body_mut().as_reader();
    let mut bytes = 0;
    let mut buffer = [0; 65536];
    loop {
        cancelled(cancel)?;
        let n = reader
            .read(&mut buffer)
            .map_err(|_| fail("vnc.download_interrupted", "下载中断"))?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > MAX_PACKAGE {
            return Err(fail("vnc.download_too_large", "文件太大"));
        }
        file.write_all(&buffer[..n])
            .map_err(|_| error("local_io"))?;
        progress(AcquireStep::Download {
            source,
            bytes,
            total,
        });
    }
    Ok(())
}

pub(super) fn unpack(
    package: &Path,
    gzip: &Path,
    expected: &str,
    cancel: &AtomicBool,
) -> Result<String, AppError> {
    let mut input = File::open(package).map_err(|_| error("local_io"))?;
    if input.metadata().map_err(|_| error("local_io"))?.len() > MAX_PACKAGE {
        return Err(error("invalid_package"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        cancelled(cancel)?;
        let n = input.read(&mut buffer).map_err(|_| error("local_io"))?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    if format!("{:x}", hash.finalize()) != expected {
        return Err(error("sha_mismatch"));
    }
    input
        .seek(SeekFrom::Start(0))
        .map_err(|_| error("local_io"))?;
    // 限制整个解压流，既不展开链接，也不信任包内路径。
    let decoder = GzDecoder::new(input).take(512 * 1024 * 1024);
    let mut archive = tar::Archive::new(decoder);
    let mut found = None;
    for entry in archive.entries().map_err(|_| error("invalid_package"))? {
        cancelled(cancel)?;
        let mut entry = entry.map_err(|_| error("invalid_package"))?;
        let path = entry.path().map_err(|_| error("invalid_package"))?;
        if path != Path::new("tigervnc-1.16.2.x86_64/usr/bin/Xvnc")
            && path != Path::new("usr/bin/Xvnc")
        {
            continue;
        }
        if found.is_some()
            || !entry.header().entry_type().is_file()
            || entry.size() == 0
            || entry.size() > MAX_BINARY
        {
            return Err(error("invalid_package"));
        }
        let mut out = GzEncoder::new(
            File::create(gzip).map_err(|_| error("local_io"))?,
            Compression::default(),
        );
        let mut hash = Sha256::new();
        loop {
            cancelled(cancel)?;
            let n = entry
                .read(&mut buffer)
                .map_err(|_| error("invalid_package"))?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            out.write_all(&buffer[..n]).map_err(|_| error("local_io"))?;
        }
        out.finish().map_err(|_| error("local_io"))?;
        found = Some(format!("{:x}", hash.finalize()));
    }
    found.ok_or_else(|| error("invalid_package"))
}

fn local_package(
    program: &SshProgram,
    cmd: &SshCommand,
    manual: Option<&Path>,
    source: AcquireSource,
    progress: &dyn Fn(AcquireStep),
    cancel: &AtomicBool,
) -> Result<XvncBinary, AppError> {
    // 临时数据遵循部署环境指定的位置，整个调用只有一个自动回收目录。
    #[cfg(test)]
    let tmp = crate::test_support::tempdir();
    #[cfg(not(test))]
    let root = std::env::var_os("PIERLINK_TEST_TMP")
        .or_else(|| std::env::var_os("TMPDIR"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(not(test))]
    let tmp = tempfile::Builder::new()
        .prefix("pierlink-vnc-")
        .tempdir_in(root)
        .map_err(|_| error("local_io"))?;
    let package = tmp.path().join("package.tar.gz");
    let package = if let Some(path) = manual {
        path
    } else {
        download(&package, source, progress, cancel)?;
        &package
    };
    let gzip = tmp.path().join("Xvnc.gz");
    let hash = unpack(package, &gzip, SHA256, cancel)?;
    upload_package(program, cmd, &gzip, &hash, progress, cancel)
}

pub(super) fn upload_package(
    program: &SshProgram,
    cmd: &SshCommand,
    gzip: &Path,
    hash: &str,
    progress: &dyn Fn(AcquireStep),
    cancel: &AtomicBool,
) -> Result<XvncBinary, AppError> {
    let command = crate::deploy::upload_command(cmd)?;
    let staging = format!(".upload-{}", uuid::Uuid::new_v4());
    let prefix = format!(
        "private_dir \"$binroot\"\nprune_uploads\nwork=\"$binroot\"/{}\n",
        sh_quote(&staging)?
    );
    let execute = |body: &str| run(program, &command, &format!("set -e\n{prefix}\n{body}"), 60);
    let result = (|| {
        let destination = format!("\"$binroot\"/{}/unpack/Xvnc", sh_quote(&staging)?);
        crate::deploy::upload_binary_to(
            gzip,
            hash,
            &destination,
            &execute,
            &|bytes, total| progress(AcquireStep::Upload { bytes, total }),
            cancel,
        )?;
        cancelled(cancel)?;
        binary_result(execute(&format!(
            "version=1.16.2\nrelative=Xvnc\nbinary=$work/unpack/Xvnc\n{}",
            include_str!("install.sh")
        ))?)
    })();
    let cleanup = execute("safe_path \"$work\"\nrm -rf -- \"$work\"\nprintf '{\"ok\":true}'");
    match (result, cleanup) {
        (Ok(b), Ok(_)) => Ok(b),
        (Err(e), _) | (_, Err(e)) => Err(e),
    }
}
