use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::models::{Conversation, Harness};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Ok,
    Warning,
    Error,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Ok => "OK",
            RunStatus::Warning => "WARNING",
            RunStatus::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastRun {
    pub timestamp: DateTime<Utc>,
    pub status: RunStatus,
    pub new_count: usize,
    pub updated_count: usize,
    pub skipped_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationRecord {
    pub id: String,
    pub harness: Harness,
    pub project: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub message_count: usize,
    pub rel_path: String,
    pub source_mtime: u64,
    pub source_size: u64,
    pub content_sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetState {
    pub conversations: BTreeMap<String, ConversationRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupState {
    pub version: u32,
    pub targets: BTreeMap<String, TargetState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<LastRun>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_notifications: BTreeMap<String, DateTime<Utc>>,
}

impl BackupState {
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                version: 1,
                targets: BTreeMap::new(),
                last_run: None,
                last_notifications: BTreeMap::new(),
            });
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read state file {}", path.display()))?;
        let state: BackupState = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse state file {}", path.display()))?;
        Ok(state)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create state directory {}", parent.display())
            })?;
        }
        let json = serde_json::to_string_pretty(self).context("Failed to serialize state")?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, json)
            .with_context(|| format!("Failed to write temp state file {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path)
            .with_context(|| format!("Failed to commit state file {}", path.display()))?;
        Ok(())
    }

    pub fn target_mut(&mut self, target_name: &str) -> &mut TargetState {
        self.targets.entry(target_name.to_string()).or_default()
    }

    /// Returns true if `key` has not been notified within `cooldown_hours`, and records `now`.
    pub fn check_and_mark_notification(
        &mut self,
        key: &str,
        now: DateTime<Utc>,
        cooldown_hours: i64,
    ) -> bool {
        if let Some(prev) = self.last_notifications.get(key) {
            if now.signed_duration_since(*prev) < Duration::hours(cooldown_hours) {
                return false;
            }
        }
        self.last_notifications.insert(key.to_string(), now);
        true
    }

    /// Total previously tracked conversations for a given harness across all targets.
    pub fn tracked_count_for_harness(&self, harness: Harness) -> usize {
        self.targets
            .values()
            .flat_map(|t| t.conversations.values())
            .filter(|r| r.harness == harness)
            .count()
    }
}

/// Deterministic SHA-256 hash over a normalized conversation's content & key metadata.
pub fn conversation_sha256(conv: &Conversation) -> String {
    let mut hasher = Sha256::new();
    let payload = serde_json::to_vec(conv).unwrap_or_default();
    hasher.update(payload);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    hex
}
