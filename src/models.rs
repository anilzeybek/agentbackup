use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    Antigravity,
    ClaudeCode,
    Codex,
    OpenCode,
}

impl Harness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Harness::Antigravity => "antigravity",
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::OpenCode => "opencode",
        }
    }

    pub fn all() -> &'static [Harness] {
        &[
            Harness::Antigravity,
            Harness::ClaudeCode,
            Harness::Codex,
            Harness::OpenCode,
        ]
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn display_label(&self) -> &'static str {
        match self {
            Role::User => "User",
            Role::Assistant => "Assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    /// Canonical globally unique identifier, e.g. `antigravity:<uuid>`
    pub id: String,
    /// Short alphanumeric suffix used in filenames (6 chars)
    pub short_id: String,
    pub harness: Harness,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub message_count: usize,
    pub messages: Vec<Message>,
}

impl Conversation {
    pub fn canonical_id(harness: Harness, raw_id: &str) -> String {
        format!("{}:{}", harness.as_str(), raw_id)
    }

    pub fn make_short_id(raw_id: &str) -> String {
        let cleaned: String = raw_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(6)
            .collect::<String>()
            .to_lowercase();
        if cleaned.is_empty() {
            "000000".to_string()
        } else {
            cleaned
        }
    }

    pub fn date_prefix(&self) -> String {
        self.created_at
            .or(self.updated_at)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "undated".to_string())
    }

    /// Recompute `message_count` and deduplicated `models` list in order of appearance.
    pub fn refresh_derived_fields(&mut self) {
        self.message_count = self.messages.len();

        let mut seen = HashSet::new();
        let mut ordered_models = Vec::new();

        for msg in &self.messages {
            if let Some(m) = msg.model.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                if seen.insert(m.to_string()) {
                    ordered_models.push(m.to_string());
                }
            }
        }
        // Preserve any session-level models recorded by the adapter (e.g. from tool turns)
        for m in &self.models {
            let trimmed = m.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                ordered_models.push(trimmed.to_string());
            }
        }
        self.models = ordered_models;

        // Clean empty optional metadata strings to None
        self.workspace = self
            .workspace
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.git_branch = self
            .git_branch
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.harness_version = self
            .harness_version
            .take()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
    }
}

/// Push a model name into a deduplicated order-preserving vector.
pub fn push_unique_model(models: &mut Vec<String>, candidate: &str) {
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed == "<synthetic>" {
        return;
    }
    if !models.iter().any(|m| m == trimmed) {
        models.push(trimmed.to_string());
    }
}

/// A discovered conversation source on disk prior to full parsing.
#[derive(Debug, Clone)]
pub struct DiscoveredSource {
    pub id: String,
    pub raw_id: String,
    pub harness: Harness,
    /// Primary file path representing this session (e.g. transcript.jsonl or opencode.db)
    pub path: PathBuf,
    /// Modification timestamp (epoch seconds) used for fast incremental change detection
    pub source_mtime: u64,
    /// Primary source size (bytes) used for fast incremental change detection
    pub source_size: u64,
    /// Total raw disk bytes for this session (e.g. entire UUID folder for Antigravity)
    pub raw_bytes: u64,
    /// Optional adapter-specific locator (e.g. "sqlite:<session_id>" or "legacy:<path>")
    pub locator: Option<String>,
}

/// Convert a Unix timestamp in seconds or milliseconds to `DateTime<Utc>`.
pub fn datetime_from_epoch_auto(ts: i64) -> Option<DateTime<Utc>> {
    if ts <= 0 {
        return None;
    }
    if ts > 1_000_000_000_000 {
        DateTime::from_timestamp_millis(ts)
    } else {
        DateTime::from_timestamp(ts, 0)
    }
}

pub fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}
