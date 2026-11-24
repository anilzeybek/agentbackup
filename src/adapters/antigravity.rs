use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::adapters::{
    dir_size, file_mtime_and_size, infer_project_from_text_chunks, HarnessAdapter,
};
use crate::models::{
    parse_rfc3339, push_unique_model, Conversation, DiscoveredSource, Harness, Message, Role,
};

pub struct AntigravityAdapter;

impl HarnessAdapter for AntigravityAdapter {
    fn harness(&self) -> Harness {
        Harness::Antigravity
    }

    fn discover(&self, root: &Path) -> Result<Vec<DiscoveredSource>> {
        if !root.exists() {
            return Ok(Vec::new());
        }

        let mut sources = Vec::new();
        let entries = fs::read_dir(root)
            .with_context(|| format!("Failed to read Antigravity root {}", root.display()))?;

        for entry in entries.flatten() {
            let conv_dir = entry.path();
            if !conv_dir.is_dir() {
                continue;
            }
            let Some(raw_id) = conv_dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if raw_id.starts_with('.') {
                continue;
            }

            let transcript = conv_dir
                .join(".system_generated")
                .join("logs")
                .join("transcript.jsonl");
            if !transcript.is_file() {
                continue;
            }

            let (source_mtime, source_size) = file_mtime_and_size(&transcript);
            let raw_bytes = dir_size(&conv_dir);

            sources.push(DiscoveredSource {
                id: Conversation::canonical_id(Harness::Antigravity, raw_id),
                raw_id: raw_id.to_string(),
                harness: Harness::Antigravity,
                path: transcript,
                source_mtime,
                source_size,
                raw_bytes,
                locator: None,
            });
        }

        sources.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(sources)
    }

    fn parse(&self, source: &DiscoveredSource) -> Result<Option<Conversation>> {
        let file = File::open(&source.path).with_context(|| {
            format!(
                "Failed to open Antigravity transcript {}",
                source.path.display()
            )
        })?;
        let reader = BufReader::new(file);

        let mut messages: Vec<Message> = Vec::new();
        let mut models: Vec<String> = Vec::new();
        let mut path_hint_chunks: Vec<String> = Vec::new();
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;
        let mut git_branch: Option<String> = None;
        let mut harness_version: Option<String> = None;

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
                .get("created_at")
                .and_then(|v| v.as_str())
                .and_then(parse_rfc3339);

            if let Some(t) = ts {
                if created_at.is_none() || Some(t) < created_at {
                    created_at = Some(t);
                }
                if updated_at.is_none() || Some(t) > updated_at {
                    updated_at = Some(t);
                }
            }

            if git_branch.is_none() {
                if let Some(b) = val
                    .get("git_branch")
                    .or_else(|| val.get("gitBranch"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    git_branch = Some(b.to_string());
                }
            }
            if harness_version.is_none() {
                if let Some(ver) = val
                    .get("harness_version")
                    .or_else(|| val.get("app_version"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    harness_version = Some(ver.to_string());
                }
            }

            let step_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let step_source = val.get("source").and_then(|v| v.as_str()).unwrap_or("");

            match step_type {
                "USER_INPUT" => {
                    if step_source == "SYSTEM" {
                        continue;
                    }
                    if let Some(content) = val.get("content").and_then(|v| v.as_str()) {
                        let c = content.trim();
                        if !c.is_empty() {
                            path_hint_chunks.push(c.to_string());
                            messages.push(Message {
                                role: Role::User,
                                content: c.to_string(),
                                timestamp: ts,
                                model: None,
                            });
                        }
                    }
                }
                "PLANNER_RESPONSE" => {
                    let msg_model = val
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());

                    if let Some(ref m) = msg_model {
                        push_unique_model(&mut models, m);
                    }

                    // Collect file paths from tool_calls solely for project/workspace inference
                    if let Some(tool_calls) = val.get("tool_calls").and_then(|v| v.as_array()) {
                        for tc in tool_calls {
                            if let Some(args) = tc.get("args") {
                                path_hint_chunks.push(args.to_string());
                            }
                        }
                    }
                    if let Some(content) = val.get("content").and_then(|v| v.as_str()) {
                        let c = content.trim();
                        if !c.is_empty() {
                            messages.push(Message {
                                role: Role::Assistant,
                                content: c.to_string(),
                                timestamp: ts,
                                model: msg_model,
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        if !messages.iter().any(|m| m.role == Role::User) {
            return Ok(None);
        }

        let (project, workspace) = match infer_project_from_text_chunks(&path_hint_chunks) {
            Some((p, pp)) => (p, Some(pp)),
            None => ("general".to_string(), None),
        };

        let mut conv = Conversation {
            id: source.id.clone(),
            short_id: Conversation::make_short_id(&source.raw_id),
            harness: Harness::Antigravity,
            project,
            workspace,
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
}
