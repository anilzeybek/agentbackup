pub mod local;

use anyhow::Result;

use crate::config::{TargetConfig, TargetType};
use crate::models::Conversation;
use crate::state::TargetState;

pub struct SyncItem<'a> {
    pub conversation: &'a Conversation,
    pub source_mtime: u64,
    pub source_size: u64,
    pub content_sha256: String,
}

#[derive(Debug, Default, Clone)]
pub struct SyncStats {
    pub new_count: usize,
    pub updated_count: usize,
    pub skipped_count: usize,
    pub total_tracked: usize,
    pub files_written: usize,
    pub bytes_written: u64,
}

/// Pluggable backup destination interface (v1 implements `LocalTarget`;
/// future versions can add GitHub/Git or other remote targets here).
pub trait BackupTarget {
    fn name(&self) -> &str;
    fn validate(&self) -> Result<()>;
    fn sync(
        &self,
        items: &[SyncItem<'_>],
        fast_skipped: usize,
        state: &mut TargetState,
        dry_run: bool,
    ) -> Result<SyncStats>;
}

pub fn build_target(cfg: &TargetConfig) -> Box<dyn BackupTarget> {
    match cfg.target_type {
        TargetType::Local => Box::new(local::LocalTarget::new(cfg.clone())),
    }
}
