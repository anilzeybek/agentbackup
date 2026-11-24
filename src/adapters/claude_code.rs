use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::adapters::{file_mtime_and_size, HarnessAdapter};
use crate::models::{
    parse_rfc3339, push_unique_model, Conversation, DiscoveredSource, Harness, Message, Role,
};
use crate::sanitize::{
    project_name_from_path, sanitize_project_name, NON_PROJECT_HOME_DIRS, WORKSPACE_CONTAINERS,
};

pub struct ClaudeCodeAdapter;

impl HarnessAdapter for ClaudeCodeAdapter {
    fn harness(&self) -> Harness {
        Harness::ClaudeCode
    }

    fn discover(&self, root: &Path) -> Result<Vec<DiscoveredSource>> {
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut sources = Vec::new();
        let proj_dirs = fs::read_dir(root)
            .with_context(|| format!("Failed to read Claude Code root {}", root.display()))?;

        for proj_entry in proj_dirs.flatten() {
            let proj_path = proj_entry.path();
            if !proj_path.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&proj_path) else {
                continue;
            };
            for file_entry in files.flatten() {
                let path = file_entry.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                // Skip subagent sidechain logs (agent-*.jsonl)
                if stem.starts_with("agent-") {
                    continue;
                }

                let (source_mtime, source_size) = file_mtime_and_size(&path);
                sources.push(DiscoveredSource {
                    id: Conversation::canonical_id(Harness::ClaudeCode, stem),
                    raw_id: stem.to_string(),
                    harness: Harness::ClaudeCode,
                    path,
                    source_mtime,
                    source_size,
                    raw_bytes: source_size,
                    locator: None,
                });
            }
        }

        sources.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sources)
    }

    fn parse(&self, source: &DiscoveredSource) -> Result<Option<Conversation>> {
        let file = File::open(&source.path).with_context(|| {
            format!(
                "Failed to open Claude Code transcript {}",
                source.path.display()
            )
        })?;
        let reader = BufReader::new(file);

        let mut messages: Vec<Message> = Vec::new();
        let mut models: Vec<String> = Vec::new();
        let mut cwd: Option<String> = None;
        let mut git_branch: Option<String> = None;
        let mut harness_version: Option<String> = None;
        let mut summary_title: Option<String> = None;
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;
        let mut last_assistant_msg_id: Option<String> = None;

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

            if val
                .get("isSidechain")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || val.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false)
            {
                continue;
            }

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
            if git_branch.is_none() {
                if let Some(b) = val
                    .get("gitBranch")
                    .or_else(|| val.get("git_branch"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    git_branch = Some(b.to_string());
                }
            }
            if harness_version.is_none() {
                if let Some(v) = val
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    harness_version = Some(v.to_string());
                }
            }

            let ts = val
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(parse_rfc3339);

            let line_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match line_type {
                "summary" => {
                    if let Some(s) = val.get("summary").and_then(|v| v.as_str()) {
                        let s = s.trim();
                        if !s.is_empty() {
                            summary_title = Some(s.to_string());
                        }
                    }
                }
                "user" => {
                    last_assistant_msg_id = None;
                    let Some(msg) = val.get("message") else {
                        continue;
                    };
                    let Some(text) = extract_claude_content(msg.get("content"), true) else {
                        continue;
                    };
                    if is_claude_internal_wrapper(&text) {
                        continue;
                    }
                    update_timestamps(&mut created_at, &mut updated_at, ts);
                    messages.push(Message {
                        role: Role::User,
                        content: text,
                        timestamp: ts,
                        model: None,
                    });
                }
                "assistant" => {
                    let Some(msg) = val.get("message") else {
                        continue;
                    };
                    let msg_model = msg
                        .get("model")
                        .or_else(|| val.get("model"))
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != "<synthetic>")
                        .map(|s| s.to_string());

                    if let Some(ref m) = msg_model {
                        push_unique_model(&mut models, m);
                    }

                    let Some(text) = extract_claude_content(msg.get("content"), false) else {
                        continue;
                    };
                    update_timestamps(&mut created_at, &mut updated_at, ts);

                    let msg_id = msg.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                    if msg_id.is_some()
                        && msg_id == last_assistant_msg_id
                        && matches!(messages.last(), Some(m) if m.role == Role::Assistant)
                    {
                        if let Some(last) = messages.last_mut() {
                            if !last.content.ends_with(&text) {
                                last.content.push_str("\n\n");
                                last.content.push_str(&text);
                            }
                            if ts.is_some() {
                                last.timestamp = ts;
                            }
                            if last.model.is_none() && msg_model.is_some() {
                                last.model = msg_model;
                            }
                        }
                    } else {
                        last_assistant_msg_id = msg_id;
                        messages.push(Message {
                            role: Role::Assistant,
                            content: text,
                            timestamp: ts,
                            model: msg_model,
                        });
                    }
                }
                _ => {}
            }
        }

        if messages.is_empty() {
            return Ok(None);
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
                    .and_then(decode_claude_encoded_dir)
            })
            .unwrap_or_else(|| "general".to_string());

        let mut conv = Conversation {
            id: source.id.clone(),
            short_id: Conversation::make_short_id(&source.raw_id),
            harness: Harness::ClaudeCode,
            project,
            workspace: cwd,
            git_branch,
            harness_version,
            models,
            title: summary_title.unwrap_or_default(),
            created_at,
            updated_at,
            message_count: 0,
            messages,
        };
        conv.refresh_derived_fields();
        Ok(Some(conv))
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

fn extract_claude_content(content: Option<&Value>, is_user: bool) -> Option<String> {
    let content = content?;
    match content {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
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
                    "document" | "file" => {
                        if is_user {
                            parts.push("[attachment omitted]".to_string());
                        }
                    }
                    // Explicitly skip tool_result, tool_use, server_tool_use, thinking, redacted_thinking
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

fn is_claude_internal_wrapper(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("<local-command-stdout>")
        || t.starts_with("<local-command-stderr>")
        || t.starts_with("<command-name>")
        || t.starts_with("<local-command-caveat>")
}

fn decode_claude_encoded_dir(encoded: &str) -> Option<String> {
    let trimmed = encoded.trim_start_matches('-');
    if trimmed.is_empty() {
        return None;
    }
    let segments: Vec<&str> = trimmed.split('-').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return None;
    }

    // Temp / system roots -> general
    if matches!(
        segments[0],
        "tmp" | "private" | "var" | "etc" | "usr" | "opt" | "System" | "Library" | "Applications"
    ) {
        return None;
    }

    if segments[0] == "Users" || segments[0] == "home" {
        // Home dir itself (`-Users-<user>`) -> general
        if segments.len() <= 2 {
            return None;
        }
        let dir1 = segments[2];
        if NON_PROJECT_HOME_DIRS.contains(&dir1) {
            return None;
        }
        if WORKSPACE_CONTAINERS.contains(&dir1) {
            // Container dir itself (`-Users-<user>-workdir`) -> general
            if segments.len() == 3 {
                return None;
            }
            return Some(sanitize_project_name(&segments[3..].join("-")));
        }
        return Some(sanitize_project_name(&segments[2..].join("-")));
    }

    Some(sanitize_project_name(trimmed))
}
