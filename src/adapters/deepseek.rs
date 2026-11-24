use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use walkdir::WalkDir;

use crate::adapters::{file_mtime_and_size, HarnessAdapter};
use crate::models::{
    parse_json_timestamp, push_unique_model, Conversation, DiscoveredSource, Harness, Message,
    ParseOutcome, Role,
};
use crate::sanitize::project_name_from_path;

pub struct DeepSeekAdapter;

impl HarnessAdapter for DeepSeekAdapter {
    fn harness(&self) -> Harness {
        Harness::DeepSeek
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
            let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };

            let is_jsonl = file_name.ends_with(".jsonl");
            let is_zst = file_name.ends_with(".jsonl.zstd") || file_name.ends_with(".jsonl.zst");
            if !is_jsonl && !is_zst {
                continue;
            }

            // If scanning `$DSH_HOME` (which may hold profiles/plugins/etc.), only pick session
            // logs under a `sessions` directory or files named `session*.jsonl*`.
            let in_sessions_dir = path
                .components()
                .any(|c| c.as_os_str() == "sessions");
            if !in_sessions_dir && !file_name.starts_with("session") {
                continue;
            }

            let raw_id = extract_dsh_session_id(path, root);
            let (source_mtime, source_size) = file_mtime_and_size(path);

            sources.push(DiscoveredSource {
                id: Conversation::canonical_id(Harness::DeepSeek, &raw_id),
                raw_id,
                harness: Harness::DeepSeek,
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
        let file = File::open(&source.path).with_context(|| {
            format!(
                "Failed to open DeepSeek Harness session {}",
                source.path.display()
            )
        })?;

        let is_zst = source
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|n| n.ends_with(".jsonl.zstd") || n.ends_with(".jsonl.zst"))
            .unwrap_or(false);

        if is_zst {
            let decoder = zstd::stream::read::Decoder::new(file).with_context(|| {
                format!(
                    "Failed to initialize zstd decoder for {}",
                    source.path.display()
                )
            })?;
            parse_dsh_reader(BufReader::new(decoder), source)
        } else {
            parse_dsh_reader(BufReader::new(file), source)
        }
    }
}

struct PendingChunk {
    text: String,
    timestamp: Option<DateTime<Utc>>,
    model: Option<String>,
}

fn parse_dsh_reader<R: BufRead>(
    reader: R,
    source: &DiscoveredSource,
) -> Result<ParseOutcome> {
    let mut messages: Vec<Message> = Vec::new();
    let mut pending_chunk: Option<PendingChunk> = None;
    let mut saw_complete_assistant_in_turn = false;

    let mut models: Vec<String> = Vec::new();
    let mut current_model: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut harness_version: Option<String> = None;
    let mut session_title: Option<String> = None;
    let mut created_at: Option<DateTime<Utc>> = None;
    let mut updated_at: Option<DateTime<Utc>> = None;

    let mut json_lines = 0usize;
    let mut recognized_events = 0usize;

    let flush_chunk = |pending: &mut Option<PendingChunk>,
                       msgs: &mut Vec<Message>,
                       saw_complete: bool| {
        if let Some(chunk) = pending.take() {
            let text = chunk.text.trim().to_string();
            if !saw_complete && !text.is_empty() {
                msgs.push(Message {
                    role: Role::Assistant,
                    content: text,
                    timestamp: chunk.timestamp,
                    model: chunk.model,
                });
            }
        }
    };

    for (line_idx, line) in reader.lines().enumerate() {
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

        let ts = val
            .get("time")
            .or_else(|| val.get("timestamp"))
            .or_else(|| val.get("createdAt"))
            .and_then(parse_json_timestamp);

        let etype = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let data = val.get("data").unwrap_or(&val);

        // Check line 1 physical metadata header (which may not have a `type` or may use `session/header`)
        if line_idx == 0 && (looks_like_dsh_header(&val) || looks_like_dsh_header(data)) {
            recognized_events += 1;
            if is_dsh_subagent_header(&val) || is_dsh_subagent_header(data) {
                return Ok(ParseOutcome::Skipped);
            }
            extract_header_metadata(
                data,
                &mut cwd,
                &mut git_branch,
                &mut harness_version,
                &mut session_title,
                &mut created_at,
                &mut updated_at,
            );
            extract_header_metadata(
                &val,
                &mut cwd,
                &mut git_branch,
                &mut harness_version,
                &mut session_title,
                &mut created_at,
                &mut updated_at,
            );
            if val.get("type").is_none()
                || matches!(
                    etype,
                    "session/header" | "session/init" | "session/start" | "header" | "session"
                )
            {
                continue;
            }
        }

        match etype {
            "session/header" | "session/init" | "session/start" | "header" | "session" => {
                recognized_events += 1;
                if is_dsh_subagent_header(data) || is_dsh_subagent_header(&val) {
                    return Ok(ParseOutcome::Skipped);
                }
                extract_header_metadata(
                    data,
                    &mut cwd,
                    &mut git_branch,
                    &mut harness_version,
                    &mut session_title,
                    &mut created_at,
                    &mut updated_at,
                );
            }
            "session/title" => {
                recognized_events += 1;
                if let Some(t) = data
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    session_title = Some(t.to_string());
                }
            }
            "request/header" => {
                recognized_events += 1;
                if let Some(m) = data
                    .get("header")
                    .and_then(|h| h.get("config"))
                    .and_then(|c| c.get("model"))
                    .or_else(|| data.get("config").and_then(|c| c.get("model")))
                    .or_else(|| data.get("model"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    current_model = Some(m.to_string());
                    push_unique_model(&mut models, m);
                }
            }
            "user/message" => {
                recognized_events += 1;
                flush_chunk(&mut pending_chunk, &mut messages, saw_complete_assistant_in_turn);
                saw_complete_assistant_in_turn = false;

                // Skip non-human injected user/message sources (e.g. system/subagent)
                if let Some(kind) = data
                    .get("source")
                    .and_then(|s| s.get("kind"))
                    .and_then(|v| v.as_str())
                {
                    if matches!(kind, "system" | "subagent" | "internal") {
                        continue;
                    }
                }

                let Some(text) = extract_dsh_content(data.get("content"), true) else {
                    continue;
                };
                update_timestamps(&mut created_at, &mut updated_at, ts);
                messages.push(Message {
                    role: Role::User,
                    content: text,
                    timestamp: ts,
                    model: None,
                });
            }
            "assistant/message" => {
                recognized_events += 1;
                let msg_model = data
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .or_else(|| current_model.clone());
                if let Some(ref m) = msg_model {
                    push_unique_model(&mut models, m);
                }

                let Some(text) = extract_dsh_content(data.get("content"), false) else {
                    continue;
                };
                // Clear any buffered chunks for this turn since we have the canonical message
                pending_chunk = None;
                saw_complete_assistant_in_turn = true;
                update_timestamps(&mut created_at, &mut updated_at, ts);
                messages.push(Message {
                    role: Role::Assistant,
                    content: text,
                    timestamp: ts,
                    model: msg_model,
                });
            }
            "assistant/chunk" => {
                recognized_events += 1;
                if saw_complete_assistant_in_turn {
                    continue;
                }
                let text_piece = extract_dsh_chunk_text(data);
                let Some(piece) = text_piece else {
                    continue;
                };
                let msg_model = data
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .or_else(|| current_model.clone());
                if let Some(ref m) = msg_model {
                    push_unique_model(&mut models, m);
                }

                update_timestamps(&mut created_at, &mut updated_at, ts);
                match &mut pending_chunk {
                    Some(existing) => {
                        existing.text.push_str(&piece);
                        if ts.is_some() {
                            existing.timestamp = ts;
                        }
                        if existing.model.is_none() && msg_model.is_some() {
                            existing.model = msg_model;
                        }
                    }
                    None => {
                        pending_chunk = Some(PendingChunk {
                            text: piece,
                            timestamp: ts,
                            model: msg_model,
                        });
                    }
                }
            }
            "turn/end" => {
                recognized_events += 1;
                flush_chunk(&mut pending_chunk, &mut messages, saw_complete_assistant_in_turn);
                saw_complete_assistant_in_turn = false;
            }
            "turn/start"
            | "system/message"
            | "tool/call"
            | "tool/result"
            | "assistant/attempt"
            | "assistant/usage"
            | "session/meta"
            | "session/compaction"
            | "session/checkpoint"
            | "context/update" => {
                recognized_events += 1;
            }
            _ => {}
        }
    }

    flush_chunk(&mut pending_chunk, &mut messages, saw_complete_assistant_in_turn);

    if !messages.iter().any(|m| m.role == Role::User) {
        if json_lines > 0 && recognized_events == 0 {
            return Ok(ParseOutcome::UnrecognizedSchema(format!(
                "{} had {} JSON lines with no recognized DeepSeek Harness event types",
                source.path.display(),
                json_lines
            )));
        }
        return Ok(ParseOutcome::Skipped);
    }

    let project = cwd
        .as_deref()
        .and_then(project_name_from_path)
        .unwrap_or_else(|| "general".to_string());

    let mut conv = Conversation {
        id: source.id.clone(),
        short_id: Conversation::make_short_id(&source.raw_id),
        harness: Harness::DeepSeek,
        project,
        workspace: cwd,
        git_branch,
        harness_version,
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

fn looks_like_dsh_header(val: &Value) -> bool {
    val.get("cwd").is_some()
        || val.get("sessionId").is_some()
        || val.get("formatVersion").is_some()
        || matches!(
            val.get("type").and_then(|v| v.as_str()),
            Some("session/header" | "session/init" | "session/start" | "header" | "session")
        )
}

fn is_dsh_subagent_header(val: &Value) -> bool {
    let has_parent = val
        .get("parentSessionId")
        .or_else(|| val.get("parentId"))
        .or_else(|| val.get("parent_id"))
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let is_subagent_kind = val
        .get("kind")
        .or_else(|| val.get("source"))
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("subagent"))
        .unwrap_or(false);
    has_parent || is_subagent_kind
}

fn extract_header_metadata(
    obj: &Value,
    cwd: &mut Option<String>,
    git_branch: &mut Option<String>,
    harness_version: &mut Option<String>,
    session_title: &mut Option<String>,
    created_at: &mut Option<DateTime<Utc>>,
    updated_at: &mut Option<DateTime<Utc>>,
) {
    if cwd.is_none() {
        if let Some(c) = obj
            .get("cwd")
            .or_else(|| obj.get("workspace"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            *cwd = Some(c.to_string());
        }
    }
    if git_branch.is_none() {
        if let Some(b) = obj
            .get("git")
            .and_then(|g| g.get("branch"))
            .or_else(|| obj.get("gitBranch"))
            .or_else(|| obj.get("git_branch"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            *git_branch = Some(b.to_string());
        }
    }
    if harness_version.is_none() {
        if let Some(v) = obj
            .get("cliVersion")
            .or_else(|| obj.get("harnessVersion"))
            .or_else(|| obj.get("version"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            *harness_version = Some(v.to_string());
        }
    }
    if session_title.is_none() {
        if let Some(t) = obj
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            *session_title = Some(t.to_string());
        }
    }
    if let Some(ts) = obj
        .get("createdAt")
        .or_else(|| obj.get("time"))
        .or_else(|| obj.get("timestamp"))
        .and_then(parse_json_timestamp)
    {
        update_timestamps(created_at, updated_at, Some(ts));
    }
}

fn extract_dsh_content(content: Option<&Value>, is_user: bool) -> Option<String> {
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
                    "text" | "output_text" | "input_text" => {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            let t = t.trim();
                            if !t.is_empty() {
                                parts.push(t.to_string());
                            }
                        }
                    }
                    "image" | "image_url" | "input_image" => {
                        if is_user {
                            parts.push("[image omitted]".to_string());
                        }
                    }
                    "file" | "document" | "attachment" => {
                        if is_user {
                            parts.push("[attachment omitted]".to_string());
                        }
                    }
                    // Skip reasoning, thinking, tool_call, tool_use, tool_result
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

fn extract_dsh_chunk_text(data: &Value) -> Option<String> {
    if let Some(chunk) = data.get("chunk") {
        let ctype = chunk.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if ctype == "text" || ctype == "text-delta" {
            return chunk
                .get("text")
                .or_else(|| chunk.get("delta"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
        }
        return None;
    }
    data.get("text")
        .or_else(|| data.get("delta"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn extract_dsh_session_id(path: &Path, root: &Path) -> String {
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let stem = file_name
        .strip_suffix(".jsonl.zstd")
        .or_else(|| file_name.strip_suffix(".jsonl.zst"))
        .or_else(|| file_name.strip_suffix(".jsonl"))
        .unwrap_or(file_name);

    // Common layout: `$DSH_HOME/sessions/<project>/session-<uuid>/session.v4.jsonl.zstd` -> use parent dir name!
    if stem == "session" || stem.starts_with("session.v") {
        if let Some(parent_name) = path
            .parent()
            .filter(|p| *p != root)
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
        {
            if parent_name != "sessions" && !parent_name.is_empty() {
                let cleaned = parent_name
                    .strip_prefix("session-")
                    .or_else(|| parent_name.strip_prefix("session_"))
                    .unwrap_or(parent_name);
                if !cleaned.is_empty() {
                    return cleaned.to_string();
                }
            }
        }
    }

    stem.strip_prefix("session-")
        .or_else(|| stem.strip_prefix("session_"))
        .unwrap_or(stem)
        .to_string()
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
