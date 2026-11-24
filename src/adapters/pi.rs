use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use walkdir::WalkDir;

use crate::adapters::{file_mtime_and_size, HarnessAdapter};
use crate::models::{
    parse_json_timestamp, push_unique_model, Conversation, DiscoveredSource, Harness, Message,
    ParseOutcome, Role,
};
use crate::sanitize::{
    project_name_from_path, sanitize_project_name, NON_PROJECT_HOME_DIRS, WORKSPACE_CONTAINERS,
};

pub struct PiAdapter;

impl HarnessAdapter for PiAdapter {
    fn harness(&self) -> Harness {
        Harness::Pi
    }

    fn discover(&self, root: &Path) -> Result<Vec<DiscoveredSource>> {
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut sources = Vec::new();
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with('.') {
                continue;
            }

            let raw_id = extract_pi_session_id(stem);
            let (source_mtime, source_size) = file_mtime_and_size(path);

            sources.push(DiscoveredSource {
                id: Conversation::canonical_id(Harness::Pi, &raw_id),
                raw_id,
                harness: Harness::Pi,
                path: path.to_path_buf(),
                source_mtime,
                source_size,
                raw_bytes: source_size,
                locator: None,
            });
        }

        sources.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sources)
    }

    fn parse(&self, source: &DiscoveredSource) -> Result<ParseOutcome> {
        let file = File::open(&source.path)
            .with_context(|| format!("Failed to open Pi session {}", source.path.display()))?;
        let reader = BufReader::new(file);

        let mut all_entries: Vec<Value> = Vec::new();
        let mut cwd: Option<String> = None;
        let mut session_title: Option<String> = None;
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;
        let mut json_lines = 0usize;
        let mut recognized_events = 0usize;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            json_lines += 1;

            let etype = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match etype {
                "session" => {
                    recognized_events += 1;
                    if cwd.is_none() {
                        if let Some(c) = val
                            .get("cwd")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            cwd = Some(c.to_string());
                        }
                    }
                    if let Some(ts) = val.get("timestamp").and_then(parse_json_timestamp) {
                        update_timestamps(&mut created_at, &mut updated_at, Some(ts));
                    }
                }
                "message"
                | "model_change"
                | "thinking_level_change"
                | "compaction"
                | "branch_summary"
                | "session_info"
                | "context_edit"
                | "custom"
                | "custom_message"
                | "label" => {
                    recognized_events += 1;
                    all_entries.push(val);
                }
                _ => {}
            }
        }

        // Resolve the active branch in Pi's `id`/`parentId` tree (walking back from the latest
        // tree node to the root). Falls back to linear order for older flat logs without `id`.
        let active_entries = resolve_active_branch(&all_entries);

        let mut messages: Vec<Message> = Vec::new();
        let mut models: Vec<String> = Vec::new();
        let mut current_model: Option<String> = None;

        for val in active_entries {
            let etype = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let ts = val.get("timestamp").and_then(parse_json_timestamp);

            match etype {
                "session_info" => {
                    if let Some(name) = val
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        session_title = Some(name.to_string());
                    }
                }
                "model_change" => {
                    if let Some(m) = val
                        .get("modelId")
                        .or_else(|| val.get("model"))
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        current_model = Some(m.to_string());
                        push_unique_model(&mut models, m);
                    }
                }
                "message" => {
                    let Some(msg) = val.get("message") else {
                        continue;
                    };
                    let role = match msg.get("role").and_then(|v| v.as_str()) {
                        Some("user") => Role::User,
                        Some("assistant") => Role::Assistant,
                        _ => continue,
                    };

                    let msg_ts = ts.or_else(|| msg.get("timestamp").and_then(parse_json_timestamp));

                    let msg_model = if role == Role::Assistant {
                        let explicit = msg
                            .get("model")
                            .or_else(|| msg.get("modelId"))
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string());
                        let resolved = explicit.or_else(|| current_model.clone());
                        if let Some(ref m) = resolved {
                            push_unique_model(&mut models, m);
                        }
                        resolved
                    } else {
                        None
                    };

                    let Some(text) = extract_pi_content(msg.get("content"), role == Role::User)
                    else {
                        continue;
                    };

                    update_timestamps(&mut created_at, &mut updated_at, msg_ts);
                    messages.push(Message {
                        role,
                        content: text,
                        timestamp: msg_ts,
                        model: msg_model,
                    });
                }
                _ => {}
            }
        }

        if !messages.iter().any(|m| m.role == Role::User) {
            if json_lines > 0 && recognized_events == 0 {
                return Ok(ParseOutcome::UnrecognizedSchema(format!(
                    "{} had {} JSON lines with no recognized Pi session entries",
                    source.path.display(),
                    json_lines
                )));
            }
            return Ok(ParseOutcome::Skipped);
        }

        let project = cwd
            .as_deref()
            .and_then(project_name_from_path)
            .or_else(|| {
                source
                    .path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .and_then(decode_pi_encoded_dir)
            })
            .unwrap_or_else(|| "general".to_string());

        let mut conv = Conversation {
            id: source.id.clone(),
            short_id: Conversation::make_short_id(&source.raw_id),
            harness: Harness::Pi,
            project,
            workspace: cwd,
            git_branch: None,
            harness_version: None,
            models,
            title: session_title.unwrap_or_default(),
            created_at,
            updated_at,
            message_count: 0,
            messages,
        };
        conv.refresh_derived_fields();
        Ok(ParseOutcome::Conversation(conv))
    }
}

/// Walk Pi's `id`/`parentId` tree backward from the last entry with an `id`, returning
/// only the active branch in root-to-leaf chronological order.
fn resolve_active_branch(entries: &[Value]) -> Vec<&Value> {
    let mut by_id: HashMap<&str, &Value> = HashMap::new();
    let mut leaf_id: Option<&str> = None;

    for entry in entries {
        if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
            by_id.insert(id, entry);
            leaf_id = Some(id);
        }
    }

    let Some(mut cur_id) = leaf_id else {
        return entries.iter().collect();
    };

    let mut branch_rev = Vec::new();
    let mut visited: HashSet<&str> = HashSet::new();

    while visited.insert(cur_id) {
        let Some(node) = by_id.get(cur_id) else {
            break;
        };
        branch_rev.push(*node);
        match node.get("parentId").and_then(|v| v.as_str()) {
            Some(parent) if !parent.is_empty() => cur_id = parent,
            _ => break,
        }
    }

    branch_rev.reverse();
    branch_rev
}

fn extract_pi_content(content: Option<&Value>, is_user: bool) -> Option<String> {
    let content = content?;
    match content {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match btype {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            let t = t.trim();
                            if !t.is_empty() {
                                parts.push(t.to_string());
                            }
                        }
                    }
                    "image" | "image_url" => {
                        if is_user {
                            parts.push("[image omitted]".to_string());
                        }
                    }
                    "file" | "document" => {
                        if is_user {
                            parts.push("[attachment omitted]".to_string());
                        }
                    }
                    // Skip thinking, tool_call, toolCall, tool_use, toolResult, etc.
                    _ => {}
                }
            }
            let joined = parts.join("\n\n").trim().to_string();
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        _ => None,
    }
}

fn extract_pi_session_id(stem: &str) -> String {
    // Typical filename: `<timestamp>_<uuid>` (e.g. `2026-06-05T10-00-00-000Z_abc123...`)
    if let Some((_, tail)) = stem.rsplit_once('_') {
        if tail.len() >= 6 {
            return tail.to_string();
        }
    }
    stem.to_string()
}

fn decode_pi_encoded_dir(encoded: &str) -> Option<String> {
    let trimmed = encoded.trim_matches('-');
    if trimmed.is_empty() {
        return None;
    }
    let segments: Vec<&str> = trimmed.split('-').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return None;
    }
    if matches!(
        segments[0],
        "tmp" | "private" | "var" | "etc" | "usr" | "opt" | "System" | "Library" | "Applications"
    ) {
        return None;
    }
    if segments[0] == "Users" || segments[0] == "home" {
        if segments.len() <= 2 {
            return None;
        }
        let dir1 = segments[2];
        if NON_PROJECT_HOME_DIRS.contains(&dir1) {
            return None;
        }
        if WORKSPACE_CONTAINERS.contains(&dir1) {
            if segments.len() == 3 {
                return None;
            }
            return Some(sanitize_project_name(&segments[3..].join("-")));
        }
        return Some(sanitize_project_name(&segments[2..].join("-")));
    }
    Some(sanitize_project_name(trimmed))
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
