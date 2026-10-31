use super::*;
use crate::syncthing::home::{
    GuiListen, HardenSpec, HomeSpec, harden_config_xml, listening_address,
};
use model::{Manifest, id, path};
use sha2::{Digest, Sha256};

fn json<T: serde::Serialize>(value: &T) -> Result<String, AppError> {
    serde_json::to_string(value).map_err(|_| err("invalid_input"))
}
fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn assignment(name: &str, value: &str) -> Result<String, AppError> {
    Ok(format!("{name}={}\n", quote(value)?))
}
fn location(
    program: &SshProgram,
    cmd: &SshCommand,
    instance_id: &str,
) -> Result<(String, Manifest), AppError> {
    if !id(instance_id) {
        return Err(err("invalid_input"));
    }
    let v = run(
        program,
        cmd,
        &format!(
            r#"
dir="$state"/{}
safe_path "$dir/manifest.json"
[ -f "$dir/manifest.json" ] || fail not_found
printf '{{"home":'; json "$HOME"; printf ',"manifest":'; json "$(cat "$dir/manifest.json")"; printf '}}'
"#,
            quote(instance_id)?
        ),
        false,
    )?;
    let home = v["home"]
        .as_str()
        .filter(|s| path(s))
        .ok_or_else(|| err("invalid_report"))?
        .to_owned();
    let raw = v["manifest"]
        .as_str()
        .ok_or_else(|| err("invalid_report"))?;
    let mut manifest: Manifest = serde_json::from_str(raw).map_err(|_| err("invalid_report"))?;
    manifest.source_digest = digest(raw);
    // 卸载只依赖可信归属路径；端口、设备 ID 等非删除字段损坏不阻塞清理。
    if !id(&manifest.info.id)
        || !model::key(&manifest.info.instance_key)
        || manifest.info.state_dir != format!("{home}/.local/state/pierlink/{instance_id}")
        || !path(&manifest.info.binary)
        || !manifest
            .info
            .binary
            .starts_with(&format!("{home}/.local/share/pierlink/bin/"))
    {
        return Err(err("invalid_report"));
    }
    if manifest.info.id != instance_id
        || manifest.paths.len() > 128
        || manifest.paths.iter().any(|p| !model::text(p, 4096))
    {
        return Err(err("invalid_report"));
    }
    Ok((home, manifest))
}
fn read_config(program: &SshProgram, cmd: &SshCommand, dir: &str) -> Result<String, AppError> {
    let v = run(
        program,
        cmd,
        &format!(
            "{}safe_path \"$dir/home/config.xml\"\njson \"$(cat \"$dir/home/config.xml\" 2>/dev/null || :)\"",
            assignment("dir", dir)?
        ),
        false,
    )?;
    v.as_str()
        .filter(|s| s.len() <= 16 * 1024 * 1024)
        .map(str::to_owned)
        .ok_or_else(|| err("invalid_report"))
}
pub(super) fn devices(xml: &str) -> Result<Vec<String>, AppError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut depth = 0usize;
    let mut out = Vec::new();
    let mut root_seen = false;
    loop {
        use quick_xml::events::Event;
        match reader.read_event().map_err(|_| err("invalid_config"))? {
            Event::Start(e) | Event::Empty(e) => {
                if depth == 0 {
                    if root_seen || e.name().as_ref() != "configuration" {
                        return Err(err("invalid_config"));
                    }
                    root_seen = true;
                }
                if depth == 1 && e.name().as_ref() == "device" {
                    for a in e.attributes() {
                        let a = a.map_err(|_| err("invalid_config"))?;
                        if a.key.as_ref() == "id" {
                            let s = a
                                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                                .map_err(|_| err("invalid_config"))?
                                .into_owned();
                            if !model::text(&s, 128) || out.len() >= 1024 {
                                return Err(err("invalid_config"));
                            }
                            out.push(s);
                        }
                    }
                } // 后续统一展开空元素。
                depth += 1;
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
            }
            Event::Eof => {
                if depth != 0 || !root_seen {
                    return Err(err("invalid_config"));
                }
                break;
            }
            _ => {}
        }
    }
    Ok(out)
}
fn write_config(dir: &str, old: &str, new: &str) -> Result<String, AppError> {
    Ok(format!(
        r#"{}
safe_path "$dir/home/config.xml"
assert_stopped
[ "$(printf '%s' "$(cat "$dir/home/config.xml")" | sha256_of)" = {} ] || fail config_changed
temporary=$dir/home/config.xml.tmp
safe_path "$temporary"
printf '%s' {} > "$temporary"
chmod 600 "$temporary"
mv -f -- "$temporary" "$dir/home/config.xml"
temporary=
"#,
        assignment("dir", dir)?,
        quote(&digest(old))?,
        quote(new)?
    ))
}
fn metadata(manifest: &Manifest) -> Result<String, AppError> {
    let mut out = assignment("dir", &manifest.info.state_dir)?;
    for (name, value) in [
        ("manifest.json", json(manifest)?),
        ("info.json", json(&manifest.info)?),
        ("devices.json", json(&manifest.devices)?),
    ] {
        out.push_str(&format!("safe_path \"$dir/{name}\"\ntemporary=\"$dir/{name}.tmp\"\nsafe_path \"$temporary\"\nprintf '%s' {} > \"$temporary\"\nmv -f -- \"$temporary\" \"$dir/{name}\"\ntemporary=\n",quote(&value)?));
    }
    Ok(out)
}
/// 根据已确认 plan 和 secrets 创建或复用实例，返回信息和凭据应用状态。
/// secrets_applied=false 时调用方必须通过 read_api_key 读取现行密钥；升级保留服务器凭据。
pub fn create_instance(
    program: &SshProgram,
    cmd: &SshCommand,
    plan: &DeployPlan,
    secrets: &InstanceSecrets,
) -> Result<CreateOutcome, AppError> {
    if !plan.autostart_consent {
        return Err(err("consent_required"));
    }
    if plan.upgrade && !plan.upgrade_confirmed && !plan.other_devices.is_empty() {
        return Err(err("upgrade_confirmation_required"));
    }
    model::version(&plan.version)?;
    if !path(&plan.home)
        || !id(&plan.instance_id)
        || !model::key(&plan.instance_key)
        || !path(&plan.socket_path)
        || plan.socket_path.len() >= 100
    {
        return Err(err("invalid_input"));
    }
    for s in [&secrets.api_key, &secrets.gui_user, &secrets.gui_password] {
        if !model::text(s, 4096) {
            return Err(err("invalid_input"));
        }
    }
    let current = inspect(program, cmd)?;
    if current.home != plan.home || current.instance_key != plan.instance_key {
        return Err(err("instance_changed"));
    }
    if plan.socket_path
        != socket_path(
            &current.home,
            current.runtime_dir.as_deref(),
            &plan.instance_id,
        )?
    {
        return Err(err("invalid_input"));
    }
    let secrets_applied = current.instance.is_none();
    let mut previous_paths = vec![];
    let mut previous_manifest = None;
    if let Some(existing) = current.instance {
        if plan.upgrade && existing.info.id != plan.instance_id {
            return Err(err("instance_changed"));
        }
        if model::version(&existing.info.version)? > model::version(&plan.version)? {
            return Err(err("instance_newer"));
        }
        if !plan.upgrade {
            return Ok(CreateOutcome {
                info: existing.info,
                secrets_applied: false,
            });
        }
        if !plan.upgrade_confirmed
            && existing
                .devices
                .iter()
                .any(|d| Some(d) != plan.local_device_id.as_ref())
        {
            return Err(err("upgrade_confirmation_required"));
        }
        if existing
            .devices
            .iter()
            .any(|d| Some(d) != plan.local_device_id.as_ref() && !plan.other_devices.contains(d))
        {
            return Err(err("upgrade_confirmation_required"));
        }
        let latest = location(program, cmd, &existing.info.id)?.1;
        if model::version(&latest.info.version)? > model::version(&plan.version)? {
            return Err(err("instance_newer")
                .with("instance_version", &latest.info.version)
                .with("bundled_version", &plan.version));
        }
        previous_manifest = Some(latest.source_digest.clone());
        previous_paths = latest.paths;
        stop_instance(program, cmd, &existing.info.id)?;
    }
    let dir = format!("{}/.local/state/pierlink/{}", plan.home, plan.instance_id);
    let binary = format!(
        "{}/.local/share/pierlink/bin/{}/{}/syncthing",
        plan.home,
        plan.version,
        plan.arch.directory()
    );
    let manifest_guard = if let Some(hash) = previous_manifest {
        format!(
            "[ -f \"$dir/manifest.json\" ] && [ \"$(printf '%s' \"$(cat \"$dir/manifest.json\")\" | sha256_of)\" = {} ] || fail instance_changed\n",
            quote(&hash)?
        )
    } else {
        "[ ! -e \"$dir/manifest.json\" ] || fail instance_changed\n".into()
    };
    let setup = format!(
        r#"{}{}{}
[ "$HOME" = {} ] || fail instance_changed
safe_path "$dir/home/config.xml"
{manifest_guard}
safe_path "$binary"
[ -x "$binary" ] || fail binary_missing
assert_stopped
safe_path "$state/instances/$key"
if [ -f "$state/instances/$key" ] && [ "$(cat "$state/instances/$key")" != {} ]; then fail instance_changed; fi
mkdir -p -- "$state/instances"
printf '%s' "${{dir##*/}}" > "$state/instances/$key"
mkdir -p -- "$dir/home" "$dir/versions" "$dir/conflicts" "$dir/logs" "$dir/run" "$state/instances"
chmod 700 "$dir" "$dir/home" "$dir/run"
if [ {secrets_applied} = true ]; then
    printf '%s\n' {} | STNOUPGRADE=1 "$binary" generate --home="$dir/home" --no-port-probing --gui-user=pierlink-pending --gui-password=- >/dev/null 2>&1 || fail generate_failed
fi
chmod 600 "$dir/home/config.xml"
printf '{{"xml":'; json "$(cat "$dir/home/config.xml")"
printf ',"device_id":'; json "$("$binary" device-id --home="$dir/home" 2>/dev/null)"
printf ',"ports":'; json "$(ports)"
printf ',"owned_binary":'; if [ -f "${{binary%/*}}/.pierlink-created" ]; then printf true; else printf false; fi
printf '}}'
"#,
        assignment("dir", &dir)?,
        assignment("binary", &binary)?,
        assignment("key", &plan.instance_key)?,
        quote(&plan.home)?,
        quote(&plan.instance_id)?,
        quote(&secrets.gui_password)?
    );
    let v = run(program, cmd, &setup, true)?;
    let old = v["xml"].as_str().ok_or_else(|| err("invalid_report"))?;
    let device_id = v["device_id"]
        .as_str()
        .filter(|s| model::text(s, 128))
        .ok_or_else(|| err("invalid_report"))?
        .to_owned();
    if plan.upgrade
        && devices(old)?.iter().any(|d| {
            d != &device_id
                && Some(d) != plan.local_device_id.as_ref()
                && !plan.other_devices.contains(d)
        })
    {
        return Err(err("upgrade_confirmation_required"));
    }
    let port = probe_port(
        v["ports"]
            .as_str()
            .ok_or_else(|| err("cannot_probe_ports"))?,
        None,
    )?;
    let existing_key;
    let existing_user;
    let (api_key, gui_user) = if secrets_applied {
        (&secrets.api_key, &secrets.gui_user)
    } else {
        existing_key = gui_value(old, "apikey")?;
        existing_user = gui_value(old, "user")?;
        (&existing_key, &existing_user)
    };
    let spec = HomeSpec {
        home: format!("{dir}/home").into(),
        gui: GuiListen::Unix {
            path: plan.socket_path.clone().into(),
        },
        gui_user: gui_user.clone(),
        listen_port: port,
    };
    let hardened = harden_config_xml(
        old,
        &HardenSpec {
            home: &spec,
            api_key,
        },
    )
    .map_err(|_| err("invalid_config"))?;
    let info = InstanceInfo {
        id: plan.instance_id.clone(),
        version: plan.version.clone(),
        binary,
        state_dir: dir.clone(),
        socket_path: plan.socket_path.clone(),
        sync_port: port,
        device_id,
        instance_key: plan.instance_key.clone(),
        autostart_consent: true,
    };
    let mut manifest = Manifest {
        source_digest: String::new(),
        startup_wait_seconds: if plan.upgrade { 90 } else { 30 },
        info: info.clone(),
        paths: vec![
            dir,
            format!(
                "{}/.local/state/pierlink/instances/{}",
                plan.home, plan.instance_key
            ),
            format!(
                "{}/.local/share/pierlink/bin/{}/{}",
                plan.home,
                plan.version,
                plan.arch.directory()
            ),
            format!(
                "{}/.config/systemd/user/pierlink-{}.service",
                plan.home, plan.instance_id
            ),
        ],
        devices: devices(&hardened)?,
    };
    let fallback = format!(
        "{}/.local/state/pierlink/r/{}",
        plan.home,
        &plan.instance_id[..8]
    );
    if plan.socket_path == format!("{fallback}/api.sock") {
        manifest.paths.push(fallback);
    }
    if v["owned_binary"] != true {
        manifest
            .paths
            .retain(|p| info.binary.strip_suffix("/syncthing") != Some(p.as_str()));
    }
    for p in previous_paths {
        if !manifest.paths.contains(&p) {
            manifest.paths.push(p);
        }
    }
    let identity_check = format!(
        "if [ -f \"$state/instances/{}\" ] && [ \"$(cat \"$state/instances/{}\")\" != {} ]; then fail instance_changed; fi\n",
        plan.instance_key,
        plan.instance_key,
        quote(&info.id)?
    );
    let body = format!(
        "{}{}{}safe_path \"$state/instances/{}\"\nprintf '%s' {} > \"$state/instances/{}\"\nprintf '{{\"ok\":true}}'",
        assignment("dir", &info.state_dir)? + &manifest_guard,
        identity_check.clone() + &write_config(&info.state_dir, old, &hardened)?,
        metadata(&manifest)?,
        plan.instance_key,
        quote(&info.id)?,
        plan.instance_key
    );
    run(program, cmd, &body, true)?;
    Ok(CreateOutcome {
        info,
        secrets_applied,
    })
}

fn service_arg(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}
pub(super) fn start_script(
    info: &InstanceInfo,
    home: &str,
    wait_seconds: u64,
) -> Result<String, AppError> {
    let args = vec![
        info.binary.clone(),
        "serve".into(),
        format!("--home={}/home", info.state_dir),
        "--no-browser".into(),
        "--no-restart".into(),
        "--no-upgrade".into(),
        format!("--gui-address=unix://{}", info.socket_path),
        format!("--log-file={}/logs/syncthing.log", info.state_dir),
        "--log-max-size=10485760".into(),
        "--log-max-old-files=1".into(),
    ];
    let command = args
        .iter()
        .map(|s| quote(s))
        .collect::<Result<Vec<_>, _>>()?
        .join(" ");
    let runtime_directive = if info
        .socket_path
        .starts_with(&format!("{home}/.local/state/pierlink/"))
    {
        String::new()
    } else {
        format!(
            "RuntimeDirectory=pierlink-{}\nRuntimeDirectoryMode=0700\n",
            &info.id[..8]
        )
    };
    let unit = format!(
        "[Unit]\nDescription=Pierlink 同步服务\n[Service]\nRestart=on-failure\nRestartSec=10\nEnvironment=STNOUPGRADE=1\n{runtime_directive}ExecStart={}\n[Install]\nWantedBy=default.target\n",
        args.iter()
            .map(|s| service_arg(s))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let service = format!("{home}/.config/systemd/user/pierlink-{}.service", info.id);
    let socket_parent = info
        .socket_path
        .rsplit_once('/')
        .ok_or_else(|| err("invalid_input"))?
        .0;
    Ok(format!(
        r#"{}{}{}
# 运行目录必须归当前用户所有且不是符号链接。
if [ "$socket_dir" != "$state/r/${{instance_id%${{instance_id#????????}}}}" ] && [ "$socket_dir" != "$dir/run" ] && [ "$socket_dir" != "$runtime/pierlink-{}" ]; then fail unsafe_path; fi
case "$socket_dir" in "$state"/*) safe_path "$socket_dir";; esac
if [ -e "$socket_dir" ]; then
    [ ! -L "$socket_dir" ] && [ "$(stat -c %u "$socket_dir")" = "$(id -u)" ] || fail unsafe_path
else
    mkdir -p -- "$socket_dir" || fail permission_denied
fi
[ "$(readlink -f "$socket_dir")" = "$socket_dir" ] || fail unsafe_path
chmod 700 "$socket_dir"
assert_stopped
safe_path "$dir/logs/syncthing.log"
safe_path "$dir/run/syncthing.pid"
safe_path "$dir/logs/syncthing.log.0"
# 旧日志保留一份，启动验证只读取本次日志。
if [ -f "$dir/logs/syncthing.log" ]; then mv -f -- "$dir/logs/syncthing.log" "$dir/logs/syncthing.log.0"; fi
for archive in "$dir/versions" "$dir/conflicts"; do
    safe_path "$archive"
    find "$archive" -type f -mtime +29 -exec rm -f -- {{}} \;
done
if [ "$systemd" = available ]; then
    safe_path "$service"
    mkdir -p -- "${{service%/*}}"
    temporary=$service.tmp
    safe_path "$temporary"
    printf '%s' {} > "$temporary"
    mv -f -- "$temporary" "$service"; temporary=
    systemctl --user daemon-reload >/dev/null 2>&1 || fail start_failed
    systemctl --user enable --now {} >/dev/null 2>&1 || fail start_failed
else
    command -v setsid >/dev/null || fail missing_setsid
    command -v nohup >/dev/null || fail missing_nohup
    STNOUPGRADE=1 setsid nohup {command} </dev/null >/dev/null 2>&1 &
    printf '%s' "$!" > "$dir/run/syncthing.pid"
fi
n=0
while [ "$n" -lt {wait_seconds} ]; do
    if [ -f "$dir/logs/syncthing.log" ] && grep -E 'GUI and API listening|address already in use' "$dir/logs/syncthing.log" >/dev/null; then break; fi
    n=$((n+1)); sleep 1
done
sleep 1
printf '{{"systemd":'; json "$systemd"; printf ',"linger":%s,"kill":%s,"taken":%s,"log":' "$linger" "$kill_users" "$taken_over"
json "$(tail -n 200 "$dir/logs/syncthing.log" 2>/dev/null || :)"; printf '}}'
"#,
        assignment("dir", &info.state_dir)? + &assignment("instance_id", &info.id)?,
        assignment("socket_dir", socket_parent)?,
        assignment("service", &service)?,
        &info.id[..8],
        quote(&unit)?,
        quote(&format!("pierlink-{}.service", info.id))?
    ))
}
/// 启动已授权实例并核对本次日志实际地址；端口竞争时换端口重试一次，结果含最新实例信息。
pub fn start_instance(
    program: &SshProgram,
    cmd: &SshCommand,
    instance: &InstanceInfo,
) -> Result<StartOutcome, AppError> {
    let (home, mut manifest) = location(program, cmd, &instance.id)?;
    if !manifest.info.autostart_consent {
        return Err(err("consent_required"));
    }
    let mut info = manifest.info.clone();
    info.validate(&home)?;
    let status = run(
        program,
        cmd,
        &format!(
            r#"{}
running=false
unix_inode=$(awk -v p="$socket" '$4=="00010000" {{line=$0; for(i=0;i<7;i++) sub(/^[[:space:]]*[^[:space:]]+[[:space:]]*/, "", line); if(line==p) print $7}}' /proc/net/unix)
tcp_address=$(printf '0100007F:%04X' "$port")
tcp_inode=$(awk -v p="$tcp_address" '$2==p && $4=="0A" {{print $10}}' /proc/net/tcp)
for proc in /proc/[0-9]*/cmdline; do
    tr '\000' '\n' < "$proc" 2>/dev/null | grep -Fx -- "--home=$dir/home" >/dev/null || continue
    pid=${{proc#/proc/}}; pid=${{pid%/cmdline}}
    exe=$(readlink "/proc/$pid/exe" 2>/dev/null || :)
    argv0=$(tr '\000' '\n' < "$proc" 2>/dev/null | head -n 1)
    [ "$exe" = "$binary" ] || [ "$exe" = "$binary (deleted)" ] || {{ case "$exe" in *" (deleted)") [ "$argv0" = "$binary" ];; *) false;; esac; }} || continue
    unix_owned=false; tcp_owned=false
    for fd in /proc/"$pid"/fd/*; do
        link=$(readlink "$fd" 2>/dev/null || :)
        [ "$link" != "socket:[$unix_inode]" ] || unix_owned=true
        [ "$link" != "socket:[$tcp_inode]" ] || tcp_owned=true
    done
    if [ "$unix_owned" = true ] && [ "$tcp_owned" = true ]; then running=true; break; fi
done
printf '{{"running":%s}}' "$running"
"#,
            assignment("dir", &info.state_dir)?
                + &assignment("socket", &info.socket_path)?
                + &assignment("binary", &info.binary)?
                + &assignment("port", &info.sync_port.to_string())?
        ),
        false,
    )?;
    if status["running"] == true {
        return Ok(StartOutcome::AlreadyRunning { instance: info });
    }
    stop_instance(program, cmd, &info.id)?;
    for attempt in 0..2 {
        let v = match run(
            program,
            cmd,
            &start_script(&info, &home, manifest.startup_wait_seconds.clamp(30, 90))?,
            true,
        ) {
            Ok(v) => v,
            Err(e) => {
                let _ = stop_instance(program, cmd, &info.id);
                return Err(e);
            }
        };
        let log = v["log"].as_str().ok_or_else(|| err("invalid_report"))?;
        if log.contains("address already in use") && attempt == 0 {
            stop_instance(program, cmd, &info.id)?;
            let ports = run(program, cmd, "json \"$(ports)\"", false)?;
            let port = probe_port(
                ports.as_str().ok_or_else(|| err("cannot_probe_ports"))?,
                Some(info.sync_port),
            )?;
            let old = read_config(program, cmd, &info.state_dir)?;
            let new = old.replace(
                &format!("tcp://127.0.0.1:{}", info.sync_port),
                &format!("tcp://127.0.0.1:{port}"),
            );
            if old == new {
                return Err(err("invalid_config"));
            }
            info.sync_port = port;
            manifest.info = info.clone();
            run(
                program,
                cmd,
                &format!(
                    "{}{}printf true",
                    write_config(&info.state_dir, &old, &new)?,
                    metadata(&manifest)?
                ),
                true,
            )?;
            manifest.source_digest = digest(&json(&manifest)?);
            continue;
        }
        if listening_address(log).as_deref() != Some(info.socket_path.as_str()) {
            stop_instance(program, cmd, &info.id)?;
            return Err(err(if listening_address(log).is_some() {
                "unsafe_listener"
            } else {
                "start_failed"
            }));
        }
        if log.contains("Failed to listen (TCP)") || log.contains("address already in use") {
            stop_instance(program, cmd, &info.id)?;
            return Err(err("start_failed"));
        }
        if manifest.startup_wait_seconds > 30 {
            manifest.startup_wait_seconds = 30;
            run(
                program,
                cmd,
                &format!(
                    "{}[ \"$(printf '%s' \"$(cat \"$dir/manifest.json\")\" | sha256_of)\" = {} ] || fail instance_changed\n{}printf true",
                    assignment("dir", &info.state_dir)?,
                    quote(&manifest.source_digest)?,
                    metadata(&manifest)?
                ),
                true,
            )?;
        }
        let taken = v["taken"].as_bool().ok_or_else(|| err("invalid_report"))?;
        let linger = v["linger"].as_bool();
        return if v["systemd"] == "available" {
            Ok(StartOutcome::Systemd {
                instance: info,
                linger,
                lock_taken_over: taken,
            })
        } else {
            Ok(StartOutcome::Nohup {
                instance: info,
                restart_after_reboot: true,
                killed_on_logout: v["kill"] == true && linger != Some(true),
                lock_taken_over: taken,
            })
        };
    }
    Err(err("start_failed"))
}
fn stop_script(info: &InstanceInfo) -> Result<String, AppError> {
    Ok(format!(
        r#"{}
if [ "$systemd" = available ]; then systemctl --user stop {} >/dev/null 2>&1 || :; fi
# 不信任 PID 文件；只终止程序路径及 --home 都精确匹配的进程。
for proc in /proc/[0-9]*/cmdline; do
    pid=${{proc#/proc/}}; pid=${{pid%/cmdline}}
    if tr '\000' '\n' < "$proc" 2>/dev/null | grep -Fx -- "--home=$dir/home" >/dev/null && {{ exe=$(readlink "/proc/$pid/exe" 2>/dev/null || :); argv0=$(tr '\000' '\n' < "$proc" 2>/dev/null | head -n 1); [ "$exe" = "$binary" ] || [ "$exe" = "$binary (deleted)" ] || {{ case "$exe" in *" (deleted)") [ "$argv0" = "$binary" ];; *) false;; esac; }}; }}; then
        kill -TERM "$pid" 2>/dev/null || :
        n=0
        while [ -e "/proc/$pid/exe" ] && [ "$n" -lt 10 ]; do sleep 1; n=$((n+1)); done
        [ ! -e "/proc/$pid/exe" ] || fail stop_failed
    fi
done
safe_path "$dir/run/syncthing.pid"
rm -f -- "$dir/run/syncthing.pid"
"#,
        assignment("dir", &info.state_dir)? + &assignment("binary", &info.binary)?,
        quote(&format!("pierlink-{}.service", info.id))?
    ))
}
/// 按实例 UUID 停止本实例；验证进程可执行文件和 home，不干预用户自己的 Syncthing。
pub fn stop_instance(
    program: &SshProgram,
    cmd: &SshCommand,
    instance_id: &str,
) -> Result<(), AppError> {
    let (_, m) = location(program, cmd, instance_id)?;
    run(
        program,
        cmd,
        &format!("{}printf true", stop_script(&m.info)?),
        true,
    )?;
    Ok(())
}
/// 读取实例清单并返回删除预览；dry_run=false 时停止实例并删除合法且未共享的清单路径。
/// 实际删除且无跳过项、其他实例或多台电脑共享时，以 rmdir 清理空父目录，保留家目录。
pub fn uninstall(
    program: &SshProgram,
    cmd: &SshCommand,
    instance_id: &str,
    dry_run: bool,
) -> Result<UninstallReport, AppError> {
    let (home, m) = location(program, cmd, instance_id)?;
    let own_manifest = format!("{}/manifest.json", m.info.state_dir);
    let others = run(
        program,
        cmd,
        &format!(
            r#"{}
snapshot=$(manifest_snapshot)
printf '{{"snapshot":'; json "$snapshot"; printf ',"manifests":['
separator=
for other in "$state"/*/manifest.json; do
    [ -f "$other" ] || continue
    [ "$other" != "$own_manifest" ] || continue
    printf '%s' "$separator"; separator=,
    json "$(cat "$other" 2>/dev/null || :)"
done
printf ']}}'
"#,
            assignment("own_manifest", &own_manifest)?
        ),
        false,
    )?;
    let snapshot = others["snapshot"]
        .as_str()
        .filter(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| err("invalid_report"))?
        .to_owned();
    let raw: Vec<String> = decode(others["manifests"].clone())?;
    let others: Vec<_> = raw
        .iter()
        .map(|s| serde_json::from_str::<Manifest>(s))
        .collect();
    let unknown_references = others.iter().any(|m| {
        m.as_ref()
            .map_or(true, |m| m.info.binary.is_empty() || m.paths.is_empty())
    });
    let device_list = read_config(program, cmd, &m.info.state_dir).and_then(|xml| devices(&xml));
    let mut report = UninstallReport {
        will_delete: vec![],
        skipped: vec![],
        devices_unavailable: device_list.is_err(),
        devices: device_list.unwrap_or_default(),
        dry_run,
    };
    report.devices.retain(|d| d != &m.info.device_id);
    let mut body = format!(
        "[ \"$(printf '%s' \"$(cat {} )\" | sha256_of)\" = {} ] || fail instance_changed\n",
        quote(&format!("{}/manifest.json", m.info.state_dir))?,
        quote(&m.source_digest)?
    );
    body.push_str(&assignment("own_manifest", &own_manifest)?);
    body.push_str(&format!(
        "[ \"$(manifest_snapshot)\" = {} ] || fail instance_changed\n",
        quote(&snapshot)?
    ));
    if !dry_run {
        body.push_str(&stop_script(&m.info)?);
        body.push_str(&format!("if [ \"$systemd\" = available ]; then systemctl --user disable {} >/dev/null 2>&1 || :; fi\n",quote(&format!("pierlink-{}.service",instance_id))?));
    }
    // 返回逐项状态；共享引用在持锁时再次检查。
    let mut parents = Vec::new();
    body.push_str("deleted_any=false\nprune_allowed=true\n");
    body.push_str("printf '['\nseparator=\n");
    let mut paths = m.paths.clone();
    paths.sort_by_key(|p| p == &m.info.state_dir);
    for (index, target) in paths.into_iter().enumerate() {
        if !manifest_path_allowed(&home, &target)
            || target == format!("{home}/.local/state/pierlink/deploy.lock")
            || target == format!("{home}/.local/state/pierlink/instances")
        {
            report.skipped.push(SkippedPath {
                path: target,
                reason: "不在可删除路径白名单内".into(),
            });
            continue;
        }
        if (unknown_references && owned_program_dir(&home, &target))
            || others.iter().filter_map(|m| m.as_ref().ok()).any(|other| {
                other.paths.contains(&target)
                    || other.info.binary.strip_suffix("/syncthing") == Some(target.as_str())
            })
        {
            report.skipped.push(SkippedPath {
                path: target,
                reason: "其他实例仍引用此程序或无法判断引用".into(),
            });
            continue;
        }
        // 不允许通过恶意清单删除其他实例或整个程序根。
        let exact = target == m.info.state_dir
            || target
                == format!(
                    "{home}/.local/state/pierlink/instances/{}",
                    m.info.instance_key
                )
            || target == format!("{home}/.config/systemd/user/pierlink-{instance_id}.service")
            || target == format!("{home}/.local/state/pierlink/r/{}", &instance_id[..8])
            || owned_program_dir(&home, &target);
        if !exact {
            report.skipped.push(SkippedPath {
                path: target,
                reason: "路径不属于本实例".into(),
            });
            continue;
        }
        body.push_str(&format!(
            r#"{}
reason=
# 子进程检查路径，拒绝符号链接但不中断其他预览项。
check=$( (trap - 0; safe_path "$target"; printf ok) )
if [ "$check" != ok ]; then reason=unsafe_path; fi
case "$target" in "$binroot"/*) if [ ! -f "$target/.pierlink-created" ]; then reason=not_owned; fi;; esac
"#,
            assignment("target", &target)?
        ));
        if !dry_run {
            body.push_str("deleted_current=false\n");
            body.push_str(
                "if [ -z \"$reason\" ]; then\n\
                 if [ -e \"$target\" ]; then\n\
                 rm -rf -- \"$target\" || fail permission_denied\n\
                 deleted_any=true\n\
                 deleted_current=true\n\
                 fi\n\
                 else prune_allowed=false; fi\n",
            );
            body.push_str(&format!("deleted_{index}=$deleted_current\n"));
            if let Some((parent, _)) = target.rsplit_once('/') {
                parents.push((index, parent.to_owned()));
            }
        }
        body.push_str("printf '%s' \"$separator\"; separator=,\nprintf '{\"path\":'; json \"$target\"; printf ',\"reason\":'; json \"$reason\"; printf '}'\n");
    }
    body.push_str("printf ']'\n");
    if !dry_run {
        body.push_str("if [ \"$systemd\" = available ]; then systemctl --user daemon-reload >/dev/null 2>&1 || :; fi\n");
    }
    if !dry_run
        && report.skipped.is_empty()
        && others.is_empty()
        && !report.devices_unavailable
        // 单台配对电脑是本次卸载的使用者；多台电脑时保守保留目录。
        && report.devices.len() <= 1
    {
        body.push_str(
            r#"if [ "$deleted_any" = true ] && [ "$prune_allowed" = true ]; then
# 先释放部署锁，避免它阻止状态目录清理；任何并发写入都会令 rmdir 保留非空目录。
cleanup
locked=false
prune_empty_parents() {
    prune_parent=$1
    while [ "$prune_parent" != "$HOME" ]; do
        case "$prune_parent" in "$HOME"/*) ;; *) break;; esac
        # 再次校验祖先，禁止沿符号链接清理其他位置。
        check=$( (trap - 0; safe_path "$prune_parent"; printf ok) )
        [ "$check" = ok ] || break
        rmdir -- "$prune_parent" || break
        prune_parent=${prune_parent%/*}
    done
}
"#,
        );
        for (index, parent) in parents {
            body.push_str(&format!(
                "if [ \"$deleted_{index}\" = true ]; then prune_empty_parents {}; fi\n",
                quote(&parent)?
            ));
        }
        body.push_str("fi\n");
    }
    let v = run(program, cmd, &body, !dry_run)?;
    let items: Vec<SkippedPath> = decode(v)?;
    for item in items {
        if item.reason.is_empty() {
            report.will_delete.push(item.path);
        } else {
            report.skipped.push(SkippedPath {
                path: item.path,
                reason: if item.reason == "not_owned" {
                    "程序目录并非本应用创建"
                } else if item.reason == "shared_binary" {
                    "其他实例仍引用此程序"
                } else {
                    "路径包含符号链接"
                }
                .into(),
            });
        }
    }
    Ok(report)
}

fn owned_program_dir(home: &str, target: &str) -> bool {
    let Some(suffix) = target.strip_prefix(&format!("{home}/.local/share/pierlink/bin/")) else {
        return false;
    };
    let Some((version, arch)) = suffix.split_once('/') else {
        return false;
    };
    model::version(version).is_ok() && matches!(arch, "amd64" | "arm64")
}

pub(super) fn gui_value(xml: &str, tag: &str) -> Result<String, AppError> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = Vec::new();
    let mut result = None;
    loop {
        use quick_xml::events::Event;
        match reader.read_event().map_err(|_| err("invalid_config"))? {
            Event::Start(e) => {
                if stack == ["configuration".to_owned(), "gui".to_owned()]
                    && e.name().as_ref() == tag
                {
                    if result.is_some() {
                        return Err(err("invalid_config"));
                    }
                    let value = reader
                        .read_text(e.name())
                        .map_err(|_| err("invalid_config"))?;
                    result = Some(
                        quick_xml::escape::unescape(&value)
                            .map_err(|_| err("invalid_config"))?
                            .into_owned(),
                    );
                } else {
                    stack.push(e.name().as_ref().to_owned());
                }
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(err("invalid_config"));
    }
    result.ok_or_else(|| err("invalid_config"))
}
/// 经 program/cmd 只读指定实例配置中的 API 密钥；返回 16–128 位合法密钥，错误不含凭据。
pub fn read_api_key(
    program: &SshProgram,
    cmd: &SshCommand,
    instance_id: &str,
) -> Result<String, AppError> {
    if !id(instance_id) {
        return Err(err("invalid_input"));
    }
    let v = run(
        program,
        cmd,
        &format!(
            r#"
file="$state"/{}/home/config.xml
safe_path "$file"
[ -r "$file" ] || fail invalid_api_key
value=$(tr -d '\n\r' < "$file" | sed -n 's/.*<apikey>\([^<]*\)<\/apikey>.*/\1/p')
json "$value"
"#,
            quote(instance_id)?
        ),
        false,
    )?;
    let key = v
        .as_str()
        .filter(|s| {
            (16..=128).contains(&s.len())
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
        .ok_or_else(|| err("invalid_api_key"))?;
    Ok(key.to_owned())
}
