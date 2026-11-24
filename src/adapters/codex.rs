use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use walkdir::WalkDir;

use crate::adapters::{file_mtime_and_size, HarnessAdapter};
use crate::models::{
    parse_rfc3339, push_unique_model, Conversation, DiscoveredSource, Harness, Message, Role,
};
use crate::sanitize::project_name_from_path;

pub struct CodexAdapter;

impl HarnessAdapter for CodexAdapter {
    fn harness(&self) -> Harness {
        Harness::Codex
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
            let is_zst = file_name.ends_with(".jsonl.zst");
            if !is_jsonl && !is_zst {
                continue;
            }

            let raw_id = extract_codex_session_id(file_name);
            let (source_mtime, source_size) = file_mtime_and_size(path);

            sources.push(DiscoveredSource {
                id: Conversation::canonical_id(Harness::Codex, &raw_id),
                raw_id,
                harness: Harness::Codex,
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

    fn parse(&self, source: &DiscoveredSource) -> Result<Option<Conversation>> {
        let file = File::open(&source.path).with_context(|| {
            format!("Failed to open Codex session {}", source.path.display())
        })?;

        let is_zst = source
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|n| n.ends_with(".jsonl.zst"))
            .unwrap_or(false);

        if is_zst {
            let decoder = zstd::stream::read::Decoder::new(file).with_context(|| {
                format!(
                    "Failed to initialize zstd decoder for {}",
                    source.path.display()
                )
            })?;
            parse_codex_reader(BufReader::new(decoder), source)
        } else {
            parse_codex_reader(BufReader::new(file), source)
        }
    }
}

fn parse_codex_reader<R: BufRead>(
    reader: R,
    source: &DiscoveredSource,
) -> Result<Option<Conversation>> {
    let mut response_messages: Vec<Message> = Vec::new();
    let mut event_messages: Vec<Message> = Vec::new();
    let mut models: Vec<String> = Vec::new();
    let mut current_model: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut harness_version: Option<String> = None;
    let mut session_id = source.raw_id.clone();
    let mut created_at: Option<DateTime<Utc>> = None;
    let mut updated_at: Option<DateTime<Utc>> = None;

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

        let ts = val
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(parse_rfc3339);

        let line_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match line_type {
            "session_meta" => {
                if let Some(payload) = val.get("payload") {
                    // Skip Codex subagent sessions
                    if payload
                        .get("source")
                        .and_then(|s| s.as_object())
                        .map(|o| o.contains_key("subagent"))
                        .unwrap_or(false)
                        || payload
                            .get("source")
                            .and_then(|s| s.as_str())
                            .map(|s| s.eq_ignore_ascii_case("subagent"))
                            .unwrap_or(false)
                    {
                        return Ok(None);
                    }
                    if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
                        if !id.trim().is_empty() {
                            session_id = id.to_string();
                        }
                    }
                    if cwd.is_none() {
                        if let Some(c) = payload
                            .get("cwd")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            cwd = Some(c.to_string());
                        }
                    }
                    if harness_version.is_none() {
                        if let Some(ver) = payload
                            .get("cli_version")
                            .or_else(|| payload.get("version"))
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            harness_version = Some(ver.to_string());
                        }
                    }
                    if git_branch.is_none() {
                        if let Some(branch) = payload
                            .get("git")
                            .and_then(|g| g.get("branch"))
                            .or_else(|| payload.get("git_branch"))
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            git_branch = Some(branch.to_string());
                        }
                    }
                    if let Some(m) = payload
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        current_model = Some(m.to_string());
                        push_unique_model(&mut models, m);
                    }
                    if let Some(mts) = payload
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .and_then(parse_rfc3339)
                    {
                        update_timestamps(&mut created_at, &mut updated_at, Some(mts));
                    }
                }
            }
            "turn_context" => {
                if let Some(payload) = val.get("payload") {
                    if cwd.is_none() {
                        if let Some(c) = payload
                            .get("cwd")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            cwd = Some(c.to_string());
                        }
                    }
                    if git_branch.is_none() {
                        if let Some(branch) = payload
                            .get("git")
                            .and_then(|g| g.get("branch"))
                            .or_else(|| payload.get("git_branch"))
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                        {
                            git_branch = Some(branch.to_string());
                        }
                    }
                    if let Some(m) = payload
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        current_model = Some(m.to_string());
                        push_unique_model(&mut models, m);
                    }
                }
            }
            "response_item" => {
                let Some(payload) = val.get("payload") else {
                    continue;
                };
                if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
                    continue;
                }
                let role = match payload.get("role").and_then(|v| v.as_str()) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => continue,
                };
                let Some(text) = extract_codex_content(payload.get("content")) else {
                    continue;
                };
                if role == Role::User && is_codex_injected_context(&text) {
                    continue;
                }
                let msg_model = if role == Role::Assistant {
                    let explicit = payload
                        .get("model")
                        .or_else(|| val.get("model"))
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

                update_timestamps(&mut created_at, &mut updated_at, ts);
                response_messages.push(Message {
                    role,
                    content: text,
                    timestamp: ts,
                    model: msg_model,
                });
            }
            "event_msg" => {
                let Some(payload) = val.get("payload") else {
                    continue;
                };
                let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let role = match ptype {
                    "user_message" => Role::User,
                    "agent_message" => Role::Assistant,
                    _ => continue,
                };
                if let Some(text) = payload.get("message").and_then(|v| v.as_str()) {
                    let t = text.trim();
                    if !t.is_empty() && !(role == Role::User && is_codex_injected_context(t)) {
                        let msg_model = if role == Role::Assistant {
                            let resolved = payload
                                .get("model")
                                .or_else(|| val.get("model"))
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(|s| s.to_string())
                                .or_else(|| current_model.clone());
                            if let Some(ref m) = resolved {
                                push_unique_model(&mut models, m);
                            }
                            resolved
                        } else {
                            None
                        };

                        update_timestamps(&mut created_at, &mut updated_at, ts);
                        event_messages.push(Message {
                            role,
                            content: t.to_string(),
                            timestamp: ts,
                            model: msg_model,
                        });
                    }
                }
            }
            // Legacy top-level message lines
            "message" => {
                let role = match val.get("role").and_then(|v| v.as_str()) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => continue,
                };
                if let Some(text) = extract_codex_content(val.get("content")) {
                    if !(role == Role::User && is_codex_injected_context(&text)) {
                        let msg_model = if role == Role::Assistant {
                            let resolved = val
                                .get("model")
                                .and_then(|v| v.as_str())
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(|s| s.to_string())
                                .or_else(|| current_model.clone());
                            if let Some(ref m) = resolved {
                                push_unique_model(&mut models, m);
                            }
                            resolved
                        } else {
                            None
                        };

                        update_timestamps(&mut created_at, &mut updated_at, ts);
                        response_messages.push(Message {
                            role,
                            content: text,
                            timestamp: ts,
                            model: msg_model,
                        });
                    }
                }
            }
            _ => {
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
                if harness_version.is_none() {
                    if let Some(ver) = val
                        .get("cli_version")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        harness_version = Some(ver.to_string());
                    }
                }
            }
        }
    }

    // Prefer response_item messages; fall back to event_msg to avoid duplicates
    let messages = if !response_messages.is_empty() {
        response_messages
    } else {
        event_messages
    };

    if !messages.iter().any(|m| m.role == Role::User) {
        return Ok(None);
    }

    let project = cwd
        .as_deref()
        .and_then(project_name_from_path)
        .unwrap_or_else(|| "general".to_string());

    let mut conv = Conversation {
        id: source.id.clone(),
        short_id: Conversation::make_short_id(&session_id),
        harness: Harness::Codex,
        project,
        workspace: cwd,
        git_branch,
        harness_version,
        models,
        title: String::new(),
        created_at,
        updated_at,
        message_count: 0,
        messages,
    };
    conv.refresh_derived_fields();
    Ok(Some(conv))
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

fn extract_codex_content(content: Option<&Value>) -> Option<String> {
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
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                let itype = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match itype {
                    "input_text" | "output_text" | "text" => {
                        if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                            let t = t.trim();
                            if !t.is_empty() {
                                parts.push(t.to_string());
                            }
                        }
                    }
                    "input_image" | "image" | "image_url" => {
                        parts.push("[image omitted]".to_string());
                    }
                    "input_file" | "file" => {
                        parts.push("[attachment omitted]".to_string());
                    }
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

fn is_codex_injected_context(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("# AGENTS.md instructions")
        || t.starts_with("<environment_context>") && t.ends_with("</environment_context>")
        || t.starts_with("<INSTRUCTIONS>") && t.ends_with("</INSTRUCTIONS>")
        || t.starts_with("<permissions instructions>")
}

fn extract_codex_session_id(file_name: &str) -> String {
    let stem = file_name
        .strip_suffix(".jsonl.zst")
        .or_else(|| file_name.strip_suffix(".jsonl"))
        .unwrap_or(file_name);
    if let Some(rest) = stem.strip_prefix("rollout-") {
        if rest.len() > 36 {
            let candidate = &rest[rest.len() - 36..];
            if candidate.chars().filter(|&c| c == '-').count() == 4 {
                return candidate.to_string();
            }
        }
        return rest.to_string();
    }
    stem.to_string()
}
