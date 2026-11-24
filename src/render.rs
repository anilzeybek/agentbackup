use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::OutputFormat;
use crate::models::{Conversation, Message, Role};
use crate::sanitize::slugify;
use crate::state::ConversationRecord;

pub fn relative_path_for(conv: &Conversation, format: OutputFormat) -> String {
    let ext = format.extension();
    let date = conv.date_prefix();
    let slug = slugify(&conv.title, 48);
    format!(
        "{}/{}_{}_{}_{}.{}",
        conv.project,
        date,
        conv.harness.as_str(),
        slug,
        conv.short_id,
        ext
    )
}

pub fn render_single(conv: &Conversation, format: OutputFormat) -> Result<String> {
    match format {
        OutputFormat::Markdown => Ok(render_markdown_single(conv)),
        OutputFormat::Txt => Ok(render_txt_single(conv)),
        OutputFormat::Json => {
            let s = serde_json::to_string_pretty(conv)
                .context("Failed to serialize conversation to JSON")?;
            Ok(format!("{}\n", s))
        }
    }
}

fn fmt_iso(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn fmt_turn_heading(msg: &Message, multi_model: bool) -> String {
    let mut label = msg.role.display_label().to_string();
    if msg.role == Role::Assistant && multi_model {
        if let Some(ref model) = msg.model {
            label.push_str(&format!(" ({})", model));
        }
    }
    if let Some(ts) = msg.timestamp {
        label.push_str(&format!(" · {}", ts.format("%Y-%m-%d %H:%M:%S UTC")));
    }
    label
}

fn yaml_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

fn render_markdown_single(conv: &Conversation) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("id: {}\n", conv.id));
    out.push_str(&format!("harness: {}\n", conv.harness.as_str()));
    if let Some(ref ver) = conv.harness_version {
        out.push_str(&format!("harness_version: {}\n", yaml_quote(ver)));
    }
    out.push_str(&format!("project: {}\n", conv.project));
    if let Some(ref ws) = conv.workspace {
        out.push_str(&format!("workspace: {}\n", yaml_quote(ws)));
    }
    if let Some(ref branch) = conv.git_branch {
        out.push_str(&format!("git_branch: {}\n", yaml_quote(branch)));
    }
    if !conv.models.is_empty() {
        out.push_str("models:\n");
        for m in &conv.models {
            out.push_str(&format!("  - {}\n", m));
        }
    }
    if let Some(dt) = conv.created_at {
        out.push_str(&format!("created_at: {}\n", fmt_iso(dt)));
    }
    if let Some(dt) = conv.updated_at {
        out.push_str(&format!("updated_at: {}\n", fmt_iso(dt)));
    }
    out.push_str(&format!("message_count: {}\n", conv.message_count));
    out.push_str(&format!("title: {}\n", yaml_quote(&conv.title)));
    out.push_str("---\n\n");
    out.push_str(&format!("# {}\n", conv.title));

    let multi_model = conv.models.len() > 1;
    for msg in &conv.messages {
        out.push_str(&format!(
            "\n## {}\n\n{}\n",
            fmt_turn_heading(msg, multi_model),
            msg.content
        ));
    }

    out
}

fn render_txt_single(conv: &Conversation) -> String {
    let mut out = String::new();
    out.push_str(&format!("ID: {}\n", conv.id));
    out.push_str(&format!("Harness: {}\n", conv.harness.as_str()));
    if let Some(ref ver) = conv.harness_version {
        out.push_str(&format!("Harness Version: {}\n", ver));
    }
    out.push_str(&format!("Project: {}\n", conv.project));
    if let Some(ref ws) = conv.workspace {
        out.push_str(&format!("Workspace: {}\n", ws));
    }
    if let Some(ref branch) = conv.git_branch {
        out.push_str(&format!("Git Branch: {}\n", branch));
    }
    if !conv.models.is_empty() {
        out.push_str(&format!("Models: {}\n", conv.models.join(", ")));
    }
    if let Some(dt) = conv.created_at {
        out.push_str(&format!("Created: {}\n", fmt_iso(dt)));
    }
    if let Some(dt) = conv.updated_at {
        out.push_str(&format!("Updated: {}\n", fmt_iso(dt)));
    }
    out.push_str(&format!("Messages: {}\n", conv.message_count));
    out.push_str(&format!("Title: {}\n", conv.title));
    out.push_str(&"=".repeat(80));
    out.push('\n');

    let multi_model = conv.models.len() > 1;
    for msg in &conv.messages {
        out.push_str(&format!(
            "\n[{}]\n{}\n",
            fmt_turn_heading(msg, multi_model),
            msg.content
        ));
    }

    out
}

#[derive(Debug, Serialize)]
struct IndexJson<'a> {
    total: usize,
    conversations: Vec<IndexJsonEntry<'a>>,
}

#[derive(Debug, Serialize)]
struct IndexJsonEntry<'a> {
    id: &'a str,
    harness: &'a str,
    project: &'a str,
    models: &'a [String],
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<DateTime<Utc>>,
    message_count: usize,
    file: &'a str,
}

pub fn render_index(records: &[&ConversationRecord], format: OutputFormat) -> Result<String> {
    let mut sorted: Vec<&ConversationRecord> = records.to_vec();
    sorted.sort_by(|a, b| {
        let ta = a.updated_at.or(a.created_at);
        let tb = b.updated_at.or(b.created_at);
        tb.cmp(&ta).then_with(|| a.id.cmp(&b.id))
    });

    match format {
        OutputFormat::Markdown => {
            let mut out = String::new();
            out.push_str("# Agent Backup Index\n\n");
            out.push_str(&format!("Total conversations: **{}**\n\n", sorted.len()));
            out.push_str("| Date | Harness | Project | Models | Messages | Title | File |\n");
            out.push_str("| :--- | :--- | :--- | :--- | ---: | :--- | :--- |\n");
            for rec in sorted {
                let date = rec
                    .created_at
                    .or(rec.updated_at)
                    .map(|t| t.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "undated".to_string());
                let models_col = if rec.models.is_empty() {
                    "-".to_string()
                } else {
                    rec.models.join(", ").replace('|', "\\|")
                };
                let safe_title = rec.title.replace('|', "\\|");
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} | [{}]({}) |\n",
                    date,
                    rec.harness.as_str(),
                    rec.project,
                    models_col,
                    rec.message_count,
                    safe_title,
                    rec.rel_path,
                    rec.rel_path
                ));
            }
            Ok(out)
        }
        OutputFormat::Txt => {
            let mut out = String::new();
            out.push_str("AGENT BACKUP INDEX\n");
            out.push_str(&format!("Total conversations: {}\n", sorted.len()));
            out.push_str(&"=".repeat(80));
            out.push('\n');
            for rec in sorted {
                let date = rec
                    .created_at
                    .or(rec.updated_at)
                    .map(|t| t.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "undated".to_string());
                let models_col = if rec.models.is_empty() {
                    "-".to_string()
                } else {
                    rec.models.join(", ")
                };
                out.push_str(&format!(
                    "{} | {:11} | {:16} | {} | {} msgs | {} -> {}\n",
                    date,
                    rec.harness.as_str(),
                    rec.project,
                    models_col,
                    rec.message_count,
                    rec.title,
                    rec.rel_path
                ));
            }
            Ok(out)
        }
        OutputFormat::Json => {
            let index = IndexJson {
                total: sorted.len(),
                conversations: sorted
                    .into_iter()
                    .map(|r| IndexJsonEntry {
                        id: &r.id,
                        harness: r.harness.as_str(),
                        project: &r.project,
                        models: &r.models,
                        title: &r.title,
                        created_at: r.created_at,
                        updated_at: r.updated_at,
                        message_count: r.message_count,
                        file: &r.rel_path,
                    })
                    .collect(),
            };
            let s = serde_json::to_string_pretty(&index)
                .context("Failed to serialize index to JSON")?;
            Ok(format!("{}\n", s))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{parse_rfc3339, Harness};

    #[test]
    fn renders_single_vs_multi_model_and_omits_none_fields() {
        let ts1 = parse_rfc3339("2026-11-24T16:34:52Z");
        let ts2 = parse_rfc3339("2026-11-24T16:35:05Z");
        let ts3 = parse_rfc3339("2026-11-24T16:36:10Z");

        let single = Conversation {
            id: "antigravity:single1".to_string(),
            short_id: "single".to_string(),
            harness: Harness::Antigravity,
            project: "demo".to_string(),
            workspace: Some("/Users/dev/workdir/demo".to_string()),
            git_branch: None,
            harness_version: None,
            models: vec!["gemini-4-neon-high".to_string()],
            title: "Single model test".to_string(),
            created_at: ts1,
            updated_at: ts2,
            message_count: 2,
            messages: vec![
                Message {
                    role: Role::User,
                    content: "Hi".to_string(),
                    timestamp: ts1,
                    model: None,
                },
                Message {
                    role: Role::Assistant,
                    content: "Hello!".to_string(),
                    timestamp: ts2,
                    model: Some("gemini-4-neon-high".to_string()),
                },
            ],
        };

        let md = render_single(&single, OutputFormat::Markdown).unwrap();
        assert!(md.contains("workspace: \"/Users/dev/workdir/demo\""));
        assert!(md.contains("models:\n  - gemini-4-neon-high\n"));
        assert!(md.contains("message_count: 2\n"));
        assert!(!md.contains("git_branch:"));
        assert!(!md.contains("harness_version:"));
        assert!(md.contains("## User · 2026-11-24 16:34:52 UTC"));
        assert!(md.contains("## Assistant · 2026-11-24 16:35:05 UTC"));
        assert!(!md.contains("## Assistant (gemini-4-neon-high)"));

        let txt = render_single(&single, OutputFormat::Txt).unwrap();
        assert!(txt.contains("Models: gemini-4-neon-high\n"));
        assert!(txt.contains("Workspace: /Users/dev/workdir/demo\n"));
        assert!(txt.contains("Messages: 2\n"));
        assert!(!txt.contains("Git Branch:"));
        assert!(!txt.contains("Harness Version:"));
        assert!(txt.contains("[Assistant · 2026-11-24 16:35:05 UTC]"));

        let mut multi = single.clone();
        multi.git_branch = Some("feat/auth".to_string());
        multi.harness_version = Some("2.0.1".to_string());
        multi.messages.push(Message {
            role: Role::Assistant,
            content: "Switched models.".to_string(),
            timestamp: ts3,
            model: Some("claude-sonnet-4-5".to_string()),
        });
        multi.refresh_derived_fields();
        assert_eq!(multi.message_count, 3);
        assert_eq!(
            multi.models,
            vec!["gemini-4-neon-high", "claude-sonnet-4-5"]
        );

        let multi_md = render_single(&multi, OutputFormat::Markdown).unwrap();
        assert!(multi_md.contains("git_branch: \"feat/auth\""));
        assert!(multi_md.contains("harness_version: \"2.0.1\""));
        assert!(multi_md.contains("## Assistant (gemini-4-neon-high) · 2026-11-24 16:35:05 UTC"));
        assert!(multi_md.contains("## Assistant (claude-sonnet-4-5) · 2026-11-24 16:36:10 UTC"));

        let multi_txt = render_single(&multi, OutputFormat::Txt).unwrap();
        assert!(multi_txt.contains("Git Branch: feat/auth\n"));
        assert!(multi_txt.contains("Harness Version: 2.0.1\n"));
        assert!(multi_txt.contains("Models: gemini-4-neon-high, claude-sonnet-4-5\n"));
        assert!(multi_txt.contains("[Assistant (claude-sonnet-4-5) · 2026-11-24 16:36:10 UTC]"));
    }
}
