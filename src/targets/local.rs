use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{OutputLayout, TargetConfig};
use crate::models::Conversation;
use crate::render::{relative_path_for, render_grouped_merge, render_index, render_single};
use crate::state::{ConversationRecord, TargetState};
use crate::targets::{BackupTarget, SyncItem, SyncStats};

pub struct LocalTarget {
    config: TargetConfig,
    root: PathBuf,
}

impl LocalTarget {
    pub fn new(config: TargetConfig) -> Self {
        let root = config.resolved_path();
        Self { config, root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn write_file(&self, rel_path: &str, content: &str, dry_run: bool) -> Result<u64> {
        let bytes = content.len() as u64;
        if dry_run {
            return Ok(bytes);
        }

        let abs_path = self.root.join(rel_path);
        if let Some(parent) = abs_path.parent() {
            if parent != self.root {
                fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create subdirectory {}", parent.display())
                })?;
            }
        }

        let tmp_name = format!(
            ".{}.agentbackup.tmp",
            abs_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file")
        );
        let tmp_path = abs_path.with_file_name(tmp_name);
        fs::write(&tmp_path, content)
            .with_context(|| format!("Failed to write temp file {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &abs_path)
            .with_context(|| format!("Failed to commit file {}", abs_path.display()))?;
        Ok(bytes)
    }

    fn sync_per_chat(
        &self,
        items: &[SyncItem<'_>],
        state: &mut TargetState,
        dry_run: bool,
        force: bool,
        stats: &mut SyncStats,
    ) -> Result<()> {
        let format = self.config.format;
        let expected_ext = format.extension();

        for item in items {
            let conv = item.conversation;
            let existing = state.conversations.get(&conv.id).cloned();

            if !force {
                if let Some(ref rec) = existing {
                    let target_exists = self.root.join(&rec.rel_path).is_file();
                    if target_exists && rec.content_sha256 == item.content_sha256 {
                        stats.skipped_count += 1;
                        if !dry_run {
                            if let Some(entry) = state.conversations.get_mut(&conv.id) {
                                entry.source_mtime = item.source_mtime;
                                entry.source_size = item.source_size;
                            }
                        }
                        continue;
                    }
                }
            }

            let rel_path = match &existing {
                Some(rec) if rec.rel_path.ends_with(&format!(".{}", expected_ext)) => {
                    rec.rel_path.clone()
                }
                _ => relative_path_for(conv, format, OutputLayout::PerChat),
            };

            let rendered = render_single(conv, format)?;
            let bytes = self.write_file(&rel_path, &rendered, dry_run)?;
            stats.files_written += 1;
            stats.bytes_written += bytes;

            if existing.is_some() {
                stats.updated_count += 1;
            } else {
                stats.new_count += 1;
            }

            if !dry_run {
                state.conversations.insert(
                    conv.id.clone(),
                    ConversationRecord {
                        id: conv.id.clone(),
                        harness: conv.harness,
                        project: conv.project.clone(),
                        models: conv.models.clone(),
                        title: conv.title.clone(),
                        created_at: conv.created_at,
                        updated_at: conv.updated_at,
                        message_count: conv.message_count,
                        rel_path,
                        source_mtime: item.source_mtime,
                        source_size: item.source_size,
                        content_sha256: item.content_sha256.clone(),
                    },
                );
            }
        }

        Ok(())
    }

    fn sync_grouped(
        &self,
        items: &[SyncItem<'_>],
        state: &mut TargetState,
        dry_run: bool,
        force: bool,
        stats: &mut SyncStats,
    ) -> Result<()> {
        let format = self.config.format;
        let layout = self.config.layout;

        struct GroupBatch<'a> {
            project: String,
            period: Option<String>,
            changed_convs: Vec<&'a Conversation>,
            records: Vec<(String, ConversationRecord)>,
        }

        let mut groups: BTreeMap<String, GroupBatch<'_>> = BTreeMap::new();

        for item in items {
            let conv = item.conversation;
            let rel_path = relative_path_for(conv, format, layout);
            let existing = state.conversations.get(&conv.id).cloned();
            let file_exists = self.root.join(&rel_path).is_file();

            if !force {
                if let Some(ref rec) = existing {
                    if file_exists
                        && rec.rel_path == rel_path
                        && rec.content_sha256 == item.content_sha256
                    {
                        stats.skipped_count += 1;
                        if !dry_run {
                            if let Some(entry) = state.conversations.get_mut(&conv.id) {
                                entry.source_mtime = item.source_mtime;
                                entry.source_size = item.source_size;
                            }
                        }
                        continue;
                    }
                }
            }

            if existing.is_some() {
                stats.updated_count += 1;
            } else {
                stats.new_count += 1;
            }

            let period = match layout {
                OutputLayout::Monthly => Some(conv.month_prefix()),
                OutputLayout::PerProject => None,
                OutputLayout::PerChat => unreachable!(),
            };

            let batch = groups.entry(rel_path.clone()).or_insert_with(|| GroupBatch {
                project: conv.project.clone(),
                period,
                changed_convs: Vec::new(),
                records: Vec::new(),
            });

            batch.changed_convs.push(conv);
            batch.records.push((
                conv.id.clone(),
                ConversationRecord {
                    id: conv.id.clone(),
                    harness: conv.harness,
                    project: conv.project.clone(),
                    models: conv.models.clone(),
                    title: conv.title.clone(),
                    created_at: conv.created_at,
                    updated_at: conv.updated_at,
                    message_count: conv.message_count,
                    rel_path,
                    source_mtime: item.source_mtime,
                    source_size: item.source_size,
                    content_sha256: item.content_sha256.clone(),
                },
            ));
        }

        for (rel_path, batch) in groups {
            let abs_path = self.root.join(&rel_path);
            let existing_content = if abs_path.is_file() {
                fs::read_to_string(&abs_path).ok()
            } else {
                None
            };

            let merged = render_grouped_merge(
                existing_content.as_deref(),
                &batch.project,
                batch.period.as_deref(),
                &batch.changed_convs,
                format,
            )?;

            let bytes = self.write_file(&rel_path, &merged, dry_run)?;
            stats.files_written += 1;
            stats.bytes_written += bytes;

            if !dry_run {
                for (id, rec) in batch.records {
                    state.conversations.insert(id, rec);
                }
            }
        }

        Ok(())
    }
}

impl BackupTarget for LocalTarget {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn validate(&self) -> Result<()> {
        if !self.root.exists() {
            bail!(
                "Target '{}' path '{}' does not exist. Create the directory first (or verify your sync folder is mounted) before running agentbackup.",
                self.config.name,
                self.root.display()
            );
        }
        if !self.root.is_dir() {
            bail!(
                "Target '{}' path '{}' exists but is not a directory.",
                self.config.name,
                self.root.display()
            );
        }
        Ok(())
    }

    fn sync(
        &self,
        items: &[SyncItem<'_>],
        fast_skipped: usize,
        state: &mut TargetState,
        dry_run: bool,
        force: bool,
    ) -> Result<SyncStats> {
        self.validate()?;

        let mut stats = SyncStats {
            skipped_count: fast_skipped,
            ..Default::default()
        };

        match self.config.layout {
            OutputLayout::PerChat => {
                self.sync_per_chat(items, state, dry_run, force, &mut stats)?;
            }
            OutputLayout::Monthly | OutputLayout::PerProject => {
                self.sync_grouped(items, state, dry_run, force, &mut stats)?;
            }
        }

        let index_filename = self.config.format.index_filename();
        let index_exists = self.root.join(index_filename).is_file();
        let changed = stats.new_count > 0 || stats.updated_count > 0;

        if changed || force || !index_exists {
            let records: Vec<&ConversationRecord> = state.conversations.values().collect();
            let index_content = render_index(&records, self.config.format)?;
            let bytes = self.write_file(index_filename, &index_content, dry_run)?;
            stats.files_written += 1;
            stats.bytes_written += bytes;
        }

        stats.total_tracked = if dry_run {
            state.conversations.len() + stats.new_count
        } else {
            state.conversations.len()
        };

        Ok(stats)
    }
}
