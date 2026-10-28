//! 配置快照及原子导入；只写服务器、转发、片段三张表。
use super::{Db, ForwardKind, PortForward, Server, ServerSource, sql_error, writes};
use crate::{
    error::AppError,
    profile::{
        ImportIssue, ImportReason, ImportReport, MissingIdentityFile, Profile, clear_credentials,
    },
};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

/// 在 db 的读取事务中填充 profile 的三个实体列表；返回查询或提交结果。
pub(crate) fn read(db: &Db, profile: &mut Profile) -> Result<(), AppError> {
    let tx =
        Transaction::new_unchecked(&db.conn, TransactionBehavior::Deferred).map_err(sql_error)?;
    profile.servers = db.list_servers()?;
    profile.port_forwards = db.list_port_forwards(None)?;
    profile.snippets = db.list_snippets()?;
    tx.commit().map_err(sql_error)
}

/// 将已校验的 profile 原子写入 db；返回映射及报告，错误时回滚。
pub(crate) fn import(db: &mut Db, profile: &Profile) -> Result<ImportReport, AppError> {
    let tx =
        Transaction::new_unchecked(&db.conn, TransactionBehavior::Immediate).map_err(sql_error)?;
    let mut report = ImportReport::default();
    import_servers(&tx, &db.list_servers()?, profile, &mut report)?;
    import_forwards(&tx, db.list_port_forwards(None)?, profile, &mut report)?;
    import_snippets(db, profile, &mut report)?;
    tx.commit().map_err(sql_error)?;
    Ok(report)
}

#[derive(Hash, PartialEq, Eq)]
enum ServerKey<'a> {
    Alias(Option<&'a str>),
    Manual(Option<&'a str>, u16, Option<&'a str>),
}

fn server_key(server: &Server) -> ServerKey<'_> {
    match server.source {
        ServerSource::SshConfig => ServerKey::Alias(server.ssh_alias.as_deref()),
        ServerSource::Manual => ServerKey::Manual(
            server.host.as_deref(),
            server.port.unwrap_or(22),
            server.username.as_deref(),
        ),
    }
}

fn issue(id: &str, name: &str, reason: ImportReason) -> ImportIssue {
    ImportIssue {
        id: id.into(),
        name: name.into(),
        reason,
    }
}

fn fresh_id(reserved: &mut HashSet<String>) -> String {
    loop {
        let id = uuid::Uuid::new_v4().to_string();
        if reserved.insert(id.clone()) {
            return id;
        }
    }
}

fn import_servers(
    conn: &Connection,
    existing: &[Server],
    profile: &Profile,
    report: &mut ImportReport,
) -> Result<(), AppError> {
    let mut known = HashMap::new();
    for server in existing {
        // 已有库也可能有重复配置，稳定选择列表中第一条，并保留所有原记录。
        known
            .entry(server_key(server))
            .or_insert_with(|| server.id.clone());
    }
    let source_ids: HashSet<_> = profile.servers.iter().map(|s| s.id.as_str()).collect();
    let mut reserved = existing
        .iter()
        .chain(&profile.servers)
        .map(|s| s.id.clone())
        .collect();
    let mut skipped = HashSet::new();
    let mut pending: Vec<_> = profile.servers.iter().collect();
    while !pending.is_empty() {
        let previous_len = pending.len();
        let mut waiting = vec![];
        for source in pending {
            let key = server_key(source);
            if let Some(id) = known.get(&key) {
                report.server_id_map.insert(source.id.clone(), id.clone());
                report.servers_skipped.push(issue(
                    &source.id,
                    &source.name,
                    ImportReason::Duplicate,
                ));
                continue;
            }
            if let Some(jump) = source.jump_server_id.as_deref() {
                if !source_ids.contains(jump) || skipped.contains(jump) {
                    skipped.insert(source.id.as_str());
                    report.servers_skipped.push(issue(
                        &source.id,
                        &source.name,
                        ImportReason::MissingJumpServer,
                    ));
                    continue;
                }
                if !report.server_id_map.contains_key(jump) {
                    waiting.push(source);
                    continue;
                }
            }
            let mut server = source.clone();
            server.id = fresh_id(&mut reserved);
            clear_credentials(&mut server);
            server.jump_server_id = source
                .jump_server_id
                .as_ref()
                .and_then(|id| report.server_id_map.get(id).cloned());
            writes::insert_servers(conn, &server)?;
            report
                .server_id_map
                .insert(source.id.clone(), server.id.clone());
            known.insert(key, server.id.clone());
            report.servers_added += 1;
            report_missing_keys(&server, report);
        }
        if waiting.len() == previous_len {
            for server in waiting {
                report.servers_skipped.push(issue(
                    &server.id,
                    &server.name,
                    ImportReason::JumpCycle,
                ));
            }
            break;
        }
        pending = waiting;
    }
    Ok(())
}

fn report_missing_keys(server: &Server, report: &mut ImportReport) {
    let mut seen = HashSet::new();
    for path in server.identity_file.iter().chain(&server.identity_files) {
        if seen.insert(path) && !Path::new(path).is_file() {
            report.missing_identity_files.push(MissingIdentityFile {
                server_id: server.id.clone(),
                name: server.name.clone(),
                path: path.clone(),
            });
        }
    }
}

fn listener_port(forward: &PortForward) -> Option<u16> {
    if forward.kind == ForwardKind::Remote {
        forward.remote_port
    } else {
        Some(forward.local_port)
    }
}

fn import_forwards(
    conn: &Connection,
    mut existing: Vec<PortForward>,
    profile: &Profile,
    report: &mut ImportReport,
) -> Result<(), AppError> {
    let mut reserved = existing
        .iter()
        .chain(&profile.port_forwards)
        .map(|f| f.id.clone())
        .collect();
    for source in &profile.port_forwards {
        let Some(server_id) = report.server_id_map.get(&source.server_id) else {
            report.forwards_skipped.push(issue(
                &source.id,
                &source.name,
                ImportReason::MissingServer,
            ));
            continue;
        };
        if existing.iter().any(|f| {
            f.server_id == *server_id
                && f.kind == source.kind
                && listener_port(f) == listener_port(source)
        }) {
            report
                .forwards_skipped
                .push(issue(&source.id, &source.name, ImportReason::Duplicate));
            continue;
        }
        let mut forward = source.clone();
        forward.id = fresh_id(&mut reserved);
        forward.server_id = server_id.clone();
        // 本地和 SOCKS 共用本机监听端口，跨服务器也会冲突；远程转发的本机端口只是目标。
        if forward.kind != ForwardKind::Remote
            && existing
                .iter()
                .any(|f| f.kind != ForwardKind::Remote && f.local_port == forward.local_port)
        {
            forward.enabled = false;
            report.forwards_disabled.push(issue(
                &source.id,
                &source.name,
                ImportReason::LocalPortConflict,
            ));
        }
        conn.execute("INSERT INTO port_forwards(id,server_id,name,kind,local_port,remote_host,remote_port,enabled,sort_order) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![forward.id,forward.server_id,forward.name,forward.kind,forward.local_port,forward.remote_host,forward.remote_port,forward.enabled,forward.sort_order]).map_err(sql_error)?;
        existing.push(forward);
        report.forwards_added += 1;
    }
    Ok(())
}

fn import_snippets(db: &Db, profile: &Profile, report: &mut ImportReport) -> Result<(), AppError> {
    let existing = db.list_snippets()?;
    let mut reserved = existing
        .iter()
        .chain(&profile.snippets)
        .map(|s| s.id.clone())
        .collect();
    let mut known: HashSet<_> = existing
        .iter()
        .map(|s| (s.title.as_str(), s.body.as_str()))
        .collect();
    for source in &profile.snippets {
        if !known.insert((&source.title, &source.body)) {
            report
                .snippets_skipped
                .push(issue(&source.id, &source.title, ImportReason::Duplicate));
            continue;
        }
        let mut snippet = source.clone();
        snippet.id = fresh_id(&mut reserved);
        // 此接口本身不启动事务，复用其校验与写入，仍受外层事务保护。
        db.upsert_snippet(&snippet)?;
        report.snippets_added += 1;
    }
    Ok(())
}
