pub mod adapters;
pub mod cli;
pub mod config;
pub mod models;
pub mod render;
pub mod sanitize;
pub mod schedule;
pub mod state;
pub mod targets;

use anyhow::{bail, Result};
use rayon::prelude::*;
use std::io::{BufRead, Write};
use std::path::Path;

use crate::adapters::{adapter_for, all_adapters};
use crate::config::{
    contract_tilde, detect_harnesses, expand_tilde, Config, OutputFormat, TargetConfig, TargetType,
};
use crate::models::{Conversation, DiscoveredSource, Harness};
use crate::render::render_single;
use crate::sanitize::sanitize_conversation;
use crate::state::{conversation_sha256, BackupState};
use crate::targets::{build_target, SyncItem, SyncStats};

#[derive(Debug, Clone)]
pub struct HarnessScanSummary {
    pub harness: Harness,
    pub display_path: String,
    pub exists: bool,
    pub session_count: usize,
    pub raw_bytes: u64,
    pub est_export_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct TargetRunReport {
    pub name: String,
    pub path: String,
    pub format: OutputFormat,
    pub stats: SyncStats,
}

pub fn scan_harnesses(cfg: &Config) -> Result<Vec<HarnessScanSummary>> {
    let detected = detect_harnesses(&cfg.harnesses);
    let mut summaries = Vec::with_capacity(detected.len());

    for item in detected {
        if !item.exists {
            summaries.push(HarnessScanSummary {
                harness: item.harness,
                display_path: item.display_path,
                exists: false,
                session_count: 0,
                raw_bytes: 0,
                est_export_bytes: 0,
            });
            continue;
        }

        let adapter = adapter_for(item.harness);
        let sources = adapter.discover(&item.resolved_path)?;
        let raw_bytes: u64 = sources.iter().map(|s| s.raw_bytes).sum();

        let (session_count, est_export_bytes): (usize, u64) = sources
            .par_iter()
            .filter_map(|src| {
                let mut conv = adapter.parse(src).ok().flatten()?;
                sanitize_conversation(&mut conv, cfg.redact_secrets);
                if !conv.has_user_message() {
                    return None;
                }
                let rendered = render_single(&conv, OutputFormat::Markdown).ok()?;
                Some((1usize, rendered.len() as u64))
            })
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));

        summaries.push(HarnessScanSummary {
            harness: item.harness,
            display_path: item.display_path,
            exists: true,
            session_count,
            raw_bytes,
            est_export_bytes,
        });
    }

    Ok(summaries)
}

struct ParsedConversation {
    conversation: Conversation,
    source_mtime: u64,
    source_size: u64,
    content_sha256: String,
}

pub fn run_backup(
    cfg: &Config,
    state_path: &Path,
    dry_run: bool,
) -> Result<Vec<TargetRunReport>> {
    if cfg.targets.is_empty() {
        bail!("No backup targets configured. Run `agentbackup init` first.");
    }

    // Build and strictly validate every target path before doing any work
    let built_targets: Vec<Box<dyn crate::targets::BackupTarget>> =
        cfg.targets.iter().map(build_target).collect();
    for target in &built_targets {
        target.validate()?;
    }

    let mut state = BackupState::load_or_default(state_path)?;

    // Discover sources across all configured harnesses
    let mut discovered: Vec<DiscoveredSource> = Vec::new();
    for adapter in all_adapters() {
        if let Some(root) = cfg.harnesses.path_for(adapter.harness()) {
            if root.exists() {
                let mut found = adapter.discover(&root)?;
                discovered.append(&mut found);
            }
        }
    }

    // Fast-path incremental filter: skip parsing any source whose
    // `(source_mtime, source_size)` is unchanged across ALL targets and whose output file exists.
    let mut to_parse: Vec<&DiscoveredSource> = Vec::new();
    let mut fast_skipped_per_target: Vec<usize> = vec![0; cfg.targets.len()];

    for src in &discovered {
        let mut all_targets_can_skip = src.source_mtime > 0;
        let mut target_skips = vec![false; cfg.targets.len()];

        if all_targets_can_skip {
            for (idx, tcfg) in cfg.targets.iter().enumerate() {
                let can_skip = state
                    .targets
                    .get(&tcfg.name)
                    .and_then(|ts| ts.conversations.get(&src.id))
                    .map(|rec| {
                        rec.source_mtime == src.source_mtime
                            && rec.source_size == src.source_size
                            && tcfg.resolved_path().join(&rec.rel_path).is_file()
                    })
                    .unwrap_or(false);
                target_skips[idx] = can_skip;
                if !can_skip {
                    all_targets_can_skip = false;
                }
            }
        }

        if all_targets_can_skip {
            for count in &mut fast_skipped_per_target {
                *count += 1;
            }
        } else {
            to_parse.push(src);
        }
    }

    // Parse and sanitize needed conversations in parallel
    let parsed: Vec<ParsedConversation> = to_parse
        .par_iter()
        .filter_map(|src| {
            let adapter = adapter_for(src.harness);
            let mut conv = match adapter.parse(src) {
                Ok(Some(c)) => c,
                _ => return None,
            };
            sanitize_conversation(&mut conv, cfg.redact_secrets);
            if !conv.has_user_message() {
                return None;
            }
            let sha = conversation_sha256(&conv);
            Some(ParsedConversation {
                conversation: conv,
                source_mtime: src.source_mtime,
                source_size: src.source_size,
                content_sha256: sha,
            })
        })
        .collect();

    let items: Vec<SyncItem<'_>> = parsed
        .iter()
        .map(|p| SyncItem {
            conversation: &p.conversation,
            source_mtime: p.source_mtime,
            source_size: p.source_size,
            content_sha256: p.content_sha256.clone(),
        })
        .collect();

    let mut reports = Vec::with_capacity(cfg.targets.len());
    for ((target, tcfg), fast_skipped) in built_targets
        .iter()
        .zip(cfg.targets.iter())
        .zip(fast_skipped_per_target.into_iter())
    {
        let tstate = state.target_mut(target.name());
        let stats = target.sync(&items, fast_skipped, tstate, dry_run)?;
        reports.push(TargetRunReport {
            name: tcfg.name.clone(),
            path: tcfg.path.clone(),
            format: tcfg.format,
            stats,
        });
    }

    if !dry_run {
        state.save_to(state_path)?;
    }

    Ok(reports)
}

pub fn init_config_interactive<R: BufRead, W: Write>(
    config_path: &Path,
    reader: &mut R,
    writer: &mut W,
) -> Result<Config> {
    let mut cfg = Config::default();

    writeln!(writer, "Detecting installed AI coding harnesses...\n")?;
    let detected = detect_harnesses(&cfg.harnesses);
    for d in &detected {
        let status = if d.exists { "found" } else { "not found" };
        writeln!(
            writer,
            "  {:12} {:10} {}",
            d.harness.as_str(),
            format!("[{}]", status),
            d.display_path
        )?;
    }
    writeln!(writer)?;

    // 1. Prompt for existing local backup directory (strictly must exist)
    let target_path = loop {
        write!(
            writer,
            "Local backup directory path (must already exist): "
        )?;
        writer.flush()?;
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            bail!("Aborted: no backup directory path provided.");
        }
        let raw = line.trim();
        if raw.is_empty() {
            writeln!(writer, "Please enter a directory path.")?;
            continue;
        }
        let resolved = expand_tilde(raw);
        if !resolved.exists() {
            writeln!(
                writer,
                "Error: '{}' does not exist. Create it first and enter an existing directory.",
                resolved.display()
            )?;
            continue;
        }
        if !resolved.is_dir() {
            writeln!(
                writer,
                "Error: '{}' is not a directory. Please enter a directory path.",
                resolved.display()
            )?;
            continue;
        }
        break contract_tilde(&resolved);
    };

    // 2. Prompt for export format
    let format = loop {
        write!(writer, "Export format [markdown/txt/json] (default: markdown): ")?;
        writer.flush()?;
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line.trim().is_empty() {
            break OutputFormat::Markdown;
        }
        if let Some(f) = OutputFormat::from_str_loose(&line) {
            break f;
        }
        writeln!(
            writer,
            "Invalid format '{}'. Choose markdown, txt, or json.",
            line.trim()
        )?;
    };

    cfg.targets = vec![TargetConfig {
        name: "local".to_string(),
        target_type: TargetType::Local,
        path: target_path,
        format,
    }];

    cfg.save_to(config_path)?;
    writeln!(writer, "\nSaved configuration to {}", config_path.display())?;
    writeln!(
        writer,
        "Run `agentbackup scan` to inspect sessions or `agentbackup run` to back them up."
    )?;

    Ok(cfg)
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}
