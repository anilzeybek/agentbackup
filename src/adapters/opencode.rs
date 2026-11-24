use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use crate::adapters::{file_mtime_and_size, HarnessAdapter};
use crate::models::{
    datetime_from_epoch_auto, push_unique_model, Conversation, DiscoveredSource, Harness, Message,
    Role,
};
use crate::sanitize::project_name_from_path;

pub struct OpenCodeAdapter;

impl HarnessAdapter for OpenCodeAdapter {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn discover(&self, root: &Path) -> Result<Vec<DiscoveredSource>> {
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut sources = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();

        // 1. SQLite v1.2+ (<root>/opencode.db)
        let db_path = root.join("opencode.db");
        if db_path.is_file() {
            if let Ok(sqlite_sources) = discover_sqlite_sessions(&db_path) {
                for src in sqlite_sources {
                    seen_ids.insert(src.raw_id.clone());
                    sources.push(src);
                }
            }
        }

        // 2. Legacy v1.1 JSON (<root>/storage/session/<projectID>/ses_*.json)
        let storage_dir = root.join("storage");
        if storage_dir.is_dir() {
            if let Ok(legacy_sources) = discover_legacy_sessions(&storage_dir, &seen_ids) {
                sources.extend(legacy_sources);
            }
        }

        sources.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sources)
    }

    fn parse(&self, source: &DiscoveredSource) -> Result<Option<Conversation>> {
        let locator = source.locator.as_deref().unwrap_or("");
        if let Some(session_id) = locator.strip_prefix("sqlite:") {
            parse_sqlite_session(&source.path, session_id, source)
        } else if let Some(session_file) = locator.strip_prefix("legacy:") {
            parse_legacy_session(Path::new(session_file), source)
        } else {
            Ok(None)
        }
    }
}

/// Open an OpenCode SQLite DB strictly read-only. If locked or unreadable in place,
/// copy the DB (plus WAL/SHM if present) to a temp dir and open the copy read-only.
fn open_sqlite_readonly(db_path: &Path) -> Result<(Connection, Option<TempDir>)> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if let Ok(conn) = Connection::open_with_flags(db_path, flags) {
        if conn.query_row("SELECT 1", [], |_| Ok(())).is_ok() {
            return Ok((conn, None));
        }
    }

    let tmp_dir = TempDir::new().context("Failed to create temp dir for OpenCode SQLite copy")?;
    let tmp_db = tmp_dir.path().join("opencode.db");
    fs::copy(db_path, &tmp_db).with_context(|| {
        format!(
            "Failed to copy locked OpenCode DB from {}",
            db_path.display()
        )
    })?;

    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", db_path.display(), suffix));
        if sidecar.is_file() {
            let dst = PathBuf::from(format!("{}{}", tmp_db.display(), suffix));
            let _ = fs::copy(&sidecar, &dst);
        }
    }

    let conn = Connection::open_with_flags(&tmp_db, flags)
        .with_context(|| format!("Failed to open temp OpenCode DB {}", tmp_db.display()))?;
    Ok((conn, Some(tmp_dir)))
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({})", table);
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(1)) else {
        return false;
    };
    let found = rows.flatten().any(|name| name == column);
    found
}

fn discover_sqlite_sessions(db_path: &Path) -> Result<Vec<DiscoveredSource>> {
    let (conn, _tmp) = open_sqlite_readonly(db_path)?;
    let (db_mtime, _db_size) = file_mtime_and_size(db_path);

    let has_updated = table_has_column(&conn, "session", "time_updated");
    let has_created = table_has_column(&conn, "session", "time_created");
    let has_parent_id = table_has_column(&conn, "session", "parent_id");

    let time_col = if has_updated {
        "COALESCE(s.time_updated, s.time_created, 0)"
    } else if has_created {
        "COALESCE(s.time_created, 0)"
    } else {
        "0"
    };

    let where_clause = if has_parent_id {
        "WHERE s.parent_id IS NULL OR s.parent_id = ''"
    } else {
        ""
    };

    let sql = format!(
        "SELECT s.id, {time_col}, \
         COALESCE((SELECT SUM(LENGTH(p.data)) FROM part p WHERE p.session_id = s.id), 0) + \
         COALESCE((SELECT SUM(LENGTH(m.data)) FROM message m WHERE m.session_id = s.id), 0) \
         FROM session s {where_clause}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let ts: i64 = row.get(1).unwrap_or(0);
        let bytes: i64 = row.get(2).unwrap_or(0);
        Ok((id, ts, bytes.max(0) as u64))
    })?;

    let mut sources = Vec::new();
    for row in rows.flatten() {
        let (session_id, ts, bytes) = row;
        let mtime = if ts > 1_000_000_000_000 {
            (ts / 1000) as u64
        } else if ts > 0 {
            ts as u64
        } else {
            db_mtime
        };

        sources.push(DiscoveredSource {
            id: Conversation::canonical_id(Harness::OpenCode, &session_id),
            raw_id: session_id.clone(),
            harness: Harness::OpenCode,
            path: db_path.to_path_buf(),
            source_mtime: mtime,
            source_size: bytes,
            raw_bytes: bytes,
            locator: Some(format!("sqlite:{}", session_id)),
        });
    }

    Ok(sources)
}

fn extract_opencode_model(mval: &Value) -> Option<String> {
    if let Some(m) = mval
        .get("modelID")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(m.to_string());
    }
    if let Some(m) = mval
        .get("model")
        .and_then(|v| v.get("modelID"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(m.to_string());
    }
    if let Some(m) = mval
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(m.to_string());
    }
    None
}

fn parse_sqlite_session(
    db_path: &Path,
    session_id: &str,
    source: &DiscoveredSource,
) -> Result<Option<Conversation>> {
    let (conn, _tmp) = open_sqlite_readonly(db_path)?;

    let has_directory = table_has_column(&conn, "session", "directory");
    let has_project_id = table_has_column(&conn, "session", "project_id");
    let has_title = table_has_column(&conn, "session", "title");
    let has_version = table_has_column(&conn, "session", "version");
    let has_created = table_has_column(&conn, "session", "time_created");
    let has_updated = table_has_column(&conn, "session", "time_updated");

    let select_title = if has_title { "title" } else { "''" };
    let select_dir = if has_directory { "directory" } else { "''" };
    let select_proj = if has_project_id { "project_id" } else { "''" };
    let select_ver = if has_version { "version" } else { "''" };
    let select_created = if has_created { "time_created" } else { "0" };
    let select_updated = if has_updated { "time_updated" } else { "0" };

    let session_sql = format!(
        "SELECT COALESCE({select_title}, ''), COALESCE({select_dir}, ''), \
         COALESCE({select_proj}, ''), COALESCE({select_ver}, ''), \
         COALESCE({select_created}, 0), COALESCE({select_updated}, 0) \
         FROM session WHERE id = ?1"
    );

    let (title, directory, project_id, version, sess_created, sess_updated): (
        String,
        String,
        String,
        String,
        i64,
        i64,
    ) = conn.query_row(&session_sql, [session_id], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })?;

    let mut workspace: Option<String> = if !directory.trim().is_empty() {
        Some(directory)
    } else {
        None
    };
    let harness_version: Option<String> = if !version.trim().is_empty() {
        Some(version)
    } else {
        None
    };

    if workspace.is_none() && !project_id.trim().is_empty() {
        if table_has_column(&conn, "project", "worktree") {
            if let Ok(wt) = conn.query_row(
                "SELECT worktree FROM project WHERE id = ?1",
                [&project_id],
                |r| r.get::<_, String>(0),
            ) {
                if !wt.trim().is_empty() && wt != "/" {
                    workspace = Some(wt);
                }
            }
        }
    }

    // Load messages for this session
    let msg_order = if table_has_column(&conn, "message", "time_created") {
        "ORDER BY time_created ASC, id ASC"
    } else {
        "ORDER BY id ASC"
    };
    let msg_created_col = if table_has_column(&conn, "message", "time_created") {
        "COALESCE(time_created, 0)"
    } else {
        "0"
    };
    let msg_sql = format!(
        "SELECT id, {msg_created_col}, data FROM message WHERE session_id = ?1 {msg_order}"
    );
    let mut msg_stmt = conn.prepare(&msg_sql)?;
    let raw_msgs: Vec<(String, i64, String)> = msg_stmt
        .query_map([session_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .flatten()
        .collect();

    // Load parts grouped by message_id
    let part_order = if table_has_column(&conn, "part", "time_created") {
        "ORDER BY time_created ASC, id ASC"
    } else {
        "ORDER BY id ASC"
    };
    let part_sql = format!("SELECT message_id, data FROM part WHERE session_id = ?1 {part_order}");
    let mut part_stmt = conn.prepare(&part_sql)?;
    let mut parts_by_msg: HashMap<String, Vec<String>> = HashMap::new();
    for row in part_stmt
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .flatten()
    {
        parts_by_msg.entry(row.0).or_default().push(row.1);
    }

    let mut messages = Vec::new();
    let mut models = Vec::new();
    let mut last_user_model: Option<String> = None;
    let mut created_at = datetime_from_epoch_auto(sess_created);
    let mut updated_at = datetime_from_epoch_auto(sess_updated);

    for (msg_id, msg_created, msg_data) in raw_msgs {
        let Ok(mval) = serde_json::from_str::<Value>(&msg_data) else {
            continue;
        };
        let role = match mval.get("role").and_then(|v| v.as_str()) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };

        if workspace.is_none() {
            if let Some(p) = mval
                .get("path")
                .and_then(|p| p.get("cwd").or_else(|| p.get("root")))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "/")
            {
                workspace = Some(p.to_string());
            }
        }

        let extracted_model = extract_opencode_model(&mval);
        let msg_model = match role {
            Role::User => {
                if extracted_model.is_some() {
                    last_user_model = extracted_model;
                }
                None
            }
            Role::Assistant => {
                let resolved = extracted_model.or_else(|| last_user_model.clone());
                if let Some(ref m) = resolved {
                    push_unique_model(&mut models, m);
                }
                resolved
            }
        };

        let ts = datetime_from_epoch_auto(msg_created).or_else(|| {
            mval.get("time")
                .and_then(|t| t.get("created"))
                .and_then(|v| v.as_i64())
                .and_then(datetime_from_epoch_auto)
        });

        let mut text_chunks = Vec::new();
        if let Some(raw_parts) = parts_by_msg.get(&msg_id) {
            for p_str in raw_parts {
                if let Ok(pval) = serde_json::from_str::<Value>(p_str) {
                    if let Some(chunk) = extract_opencode_part(&pval, role == Role::User) {
                        text_chunks.push(chunk);
                    }
                }
            }
        }

        let joined = text_chunks.join("\n\n").trim().to_string();
        if joined.is_empty() {
            continue;
        }
        update_timestamps(&mut created_at, &mut updated_at, ts);
        messages.push(Message {
            role,
            content: joined,
            timestamp: ts,
            model: msg_model,
        });
    }

    if !messages.iter().any(|m| m.role == Role::User) {
        return Ok(None);
    }

    let project = workspace
        .as_deref()
        .and_then(project_name_from_path)
        .unwrap_or_else(|| "general".to_string());

    let mut conv = Conversation {
        id: source.id.clone(),
        short_id: Conversation::make_short_id(session_id),
        harness: Harness::OpenCode,
        project,
        workspace,
        git_branch: None,
        harness_version,
        models,
        title,
        created_at,
        updated_at,
        message_count: 0,
        messages,
    };
    conv.refresh_derived_fields();
    Ok(Some(conv))
}

fn discover_legacy_sessions(
    storage_dir: &Path,
    seen_ids: &HashSet<String>,
) -> Result<Vec<DiscoveredSource>> {
    let session_root = storage_dir.join("session");
    if !session_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut sources = Vec::new();
    for proj_entry in fs::read_dir(&session_root)?.flatten() {
        let proj_dir = proj_entry.path();
        if !proj_dir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&proj_dir) else {
            continue;
        };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            // Skip child subagent sessions
            if val
                .get("parentID")
                .or_else(|| val.get("parent_id"))
                .and_then(|v| v.as_str())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                continue;
            }
            let session_id = val
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                });
            let Some(session_id) = session_id else {
                continue;
            };
            if seen_ids.contains(&session_id) {
                continue;
            }

            let (mut mtime, mut size) = file_mtime_and_size(&path);
            if let Some(updated) = val
                .get("time")
                .and_then(|t| t.get("updated"))
                .and_then(|v| v.as_i64())
            {
                let u_sec = if updated > 1_000_000_000_000 {
                    (updated / 1000) as u64
                } else {
                    updated.max(0) as u64
                };
                if u_sec > mtime {
                    mtime = u_sec;
                }
            }

            let msg_dir = storage_dir.join("message").join(&session_id);
            if msg_dir.is_dir() {
                if let Ok(mfiles) = fs::read_dir(&msg_dir) {
                    for mf in mfiles.flatten() {
                        let (mm, ms) = file_mtime_and_size(&mf.path());
                        mtime = mtime.max(mm);
                        size += ms;
                        if let Some(mid) = mf.path().file_stem().and_then(|s| s.to_str()) {
                            let part_dir = storage_dir.join("part").join(mid);
                            if part_dir.is_dir() {
                                if let Ok(pfiles) = fs::read_dir(&part_dir) {
                                    for pf in pfiles.flatten() {
                                        let (pm, ps) = file_mtime_and_size(&pf.path());
                                        mtime = mtime.max(pm);
                                        size += ps;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            sources.push(DiscoveredSource {
                id: Conversation::canonical_id(Harness::OpenCode, &session_id),
                raw_id: session_id,
                harness: Harness::OpenCode,
                path: path.clone(),
                source_mtime: mtime,
                source_size: size,
                raw_bytes: size,
                locator: Some(format!("legacy:{}", path.display())),
            });
        }
    }

    Ok(sources)
}

fn parse_legacy_session(
    session_file: &Path,
    source: &DiscoveredSource,
) -> Result<Option<Conversation>> {
    let storage_dir = session_file
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .context("Unexpected OpenCode legacy session path")?;

    let raw = fs::read_to_string(session_file)?;
    let sval: Value = serde_json::from_str(&raw)?;

    let session_id = sval
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or(&source.raw_id);
    let title = sval
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let workspace = sval
        .get("directory")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());
    let harness_version = sval
        .get("version")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());

    let mut created_at = sval
        .get("time")
        .and_then(|t| t.get("created"))
        .and_then(|v| v.as_i64())
        .and_then(datetime_from_epoch_auto);
    let mut updated_at = sval
        .get("time")
        .and_then(|t| t.get("updated"))
        .and_then(|v| v.as_i64())
        .and_then(datetime_from_epoch_auto);

    let msg_dir = storage_dir.join("message").join(session_id);
    if !msg_dir.is_dir() {
        return Ok(None);
    }

    let mut raw_msgs: Vec<(i64, String, Value)> = Vec::new();
    for entry in fs::read_dir(&msg_dir)?.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mval) = serde_json::from_str::<Value>(&content) else {
            continue;
        };
        let mid = mval
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        let created = mval
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        raw_msgs.push((created, mid, mval));
    }
    raw_msgs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    let mut messages = Vec::new();
    let mut models = Vec::new();
    let mut last_user_model: Option<String> = None;

    for (created, mid, mval) in raw_msgs {
        let role = match mval.get("role").and_then(|v| v.as_str()) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let ts = datetime_from_epoch_auto(created);

        let extracted_model = extract_opencode_model(&mval);
        let msg_model = match role {
            Role::User => {
                if extracted_model.is_some() {
                    last_user_model = extracted_model;
                }
                None
            }
            Role::Assistant => {
                let resolved = extracted_model.or_else(|| last_user_model.clone());
                if let Some(ref m) = resolved {
                    push_unique_model(&mut models, m);
                }
                resolved
            }
        };

        let part_dir = storage_dir.join("part").join(&mid);
        let mut raw_parts: Vec<(String, Value)> = Vec::new();
        if part_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&part_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("json")
                    {
                        continue;
                    }
                    let Ok(content) = fs::read_to_string(&path) else {
                        continue;
                    };
                    let Ok(pval) = serde_json::from_str::<Value>(&content) else {
                        continue;
                    };
                    let pid = pval
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            path.file_stem()
                                .and_then(|s| s.to_str())
                                .map(|s| s.to_string())
                        })
                        .unwrap_or_default();
                    raw_parts.push((pid, pval));
                }
            }
        }
        raw_parts.sort_by(|a, b| a.0.cmp(&b.0));

        let mut text_chunks = Vec::new();
        for (_, pval) in raw_parts {
            if let Some(chunk) = extract_opencode_part(&pval, role == Role::User) {
                text_chunks.push(chunk);
            }
        }

        let joined = text_chunks.join("\n\n").trim().to_string();
        if joined.is_empty() {
            continue;
        }
        update_timestamps(&mut created_at, &mut updated_at, ts);
        messages.push(Message {
            role,
            content: joined,
            timestamp: ts,
            model: msg_model,
        });
    }

    if !messages.iter().any(|m| m.role == Role::User) {
        return Ok(None);
    }

    let project = workspace
        .as_deref()
        .and_then(project_name_from_path)
        .unwrap_or_else(|| "general".to_string());

    let mut conv = Conversation {
        id: source.id.clone(),
        short_id: Conversation::make_short_id(session_id),
        harness: Harness::OpenCode,
        project,
        workspace,
        git_branch: None,
        harness_version,
        models,
        title,
        created_at,
        updated_at,
        message_count: 0,
        messages,
    };
    conv.refresh_derived_fields();
    Ok(Some(conv))
}

fn extract_opencode_part(part: &Value, is_user: bool) -> Option<String> {
    let ptype = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ptype {
        "text" => {
            if part
                .get("synthetic")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return None;
            }
            let text = part.get("text").and_then(|v| v.as_str())?.trim();
            if text.is_empty() {
                None
            } else {
                Some(text.to_string())
            }
        }
        "image" => {
            if is_user {
                Some("[image omitted]".to_string())
            } else {
                None
            }
        }
        "file" => {
            if !is_user {
                return None;
            }
            let mime = part.get("mime").and_then(|v| v.as_str()).unwrap_or("");
            if mime.starts_with("image/") {
                Some("[image omitted]".to_string())
            } else {
                Some("[attachment omitted]".to_string())
            }
        }
        _ => None,
    }
}

fn update_timestamps(
    created_at: &mut Option<DateTime<Utc>>,
    updated_at: &mut Option<DateTime<Utc>>,
    ts: Option<DateTime<Utc>>,
) {
    if let Some(t) = ts {
        if created_at.is_none() || Some(t) < *created_at {
            *created_at = Some(t);
        }
        if updated_at.is_none() || Some(t) > *updated_at {
            *updated_at = Some(t);
        }
    }
}
