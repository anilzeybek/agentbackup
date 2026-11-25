pub mod adapters;
pub mod cli;
pub mod config;
pub mod models;
pub mod notify;
pub mod render;
pub mod sanitize;
pub mod schedule;
pub mod state;
pub mod targets;

use anyhow::{bail, Result};
use chrono::Utc;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, Write};
use std::path::Path;

use crate::adapters::{adapter_for, all_adapters};
use crate::config::{
    contract_tilde, detect_harnesses, expand_tilde, Config, OutputFormat, TargetConfig, TargetType,
};
use crate::models::{Conversation, DiscoveredSource, Harness, ParseOutcome};
use crate::render::render_single;
use crate::sanitize::sanitize_conversation;
use crate::state::{conversation_sha256, BackupState, LastRun, RunStatus};
use crate::targets::{build_target, SyncItem, SyncStats};

pub const NOTIFICATION_COOLDOWN_HOURS: i64 = 24;

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

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub reports: Vec<TargetRunReport>,
    pub warnings: Vec<String>,
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
                let mut conv = match adapter.parse(src) {
                    Ok(ParseOutcome::Conversation(c)) => c,
                    _ => return None,
                };
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

#[allow(clippy::large_enum_variant)]
enum ParseJobResult {
    Parsed(ParsedConversation),
    Skipped,
    Unrecognized(Harness, String),
    Error(Harness, String),
}

fn dir_has_non_hidden_entries(root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.file_name()
            .to_str()
            .map(|n| !n.starts_with('.'))
            .unwrap_or(false)
    })
}

pub fn run_backup(cfg: &Config, state_path: &Path, dry_run: bool) -> Result<RunOutcome> {
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
    let mut warnings: Vec<String> = Vec::new();

    // Discover sources across all configured harnesses (collecting drift warnings per harness)
    let mut discovered: Vec<DiscoveredSource> = Vec::new();
    for adapter in all_adapters() {
        let harness = adapter.harness();
        let Some(root) = cfg.harnesses.path_for(harness) else {
            continue;
        };
        if !root.exists() {
            continue;
        }
        match adapter.discover(&root) {
            Ok(mut found) => {
                if found.is_empty() && dir_has_non_hidden_entries(&root) {
                    let prev_tracked = state.tracked_count_for_harness(harness);
                    if prev_tracked > 0 {
                        warnings.push(format!(
                            "{}: directory has entries but 0 sessions were discovered (previously tracked {}) — storage layout may have changed",
                            harness.as_str(),
                            prev_tracked
                        ));
                    }
                }
                discovered.append(&mut found);
            }
            Err(err) => {
                warnings.push(format!(
                    "{}: failed to inspect {} ({}) — storage schema may have changed",
                    harness.as_str(),
                    contract_tilde(&root),
                    err
                ));
            }
        }
    }

    // Fast-path incremental filter: skip parsing any source whose
    // `(source_mtime, source_size)` is unchanged across ALL targets and whose output file exists.
    let mut to_parse: Vec<&DiscoveredSource> = Vec::new();
    let mut fast_skipped_per_target: Vec<usize> = vec![0; cfg.targets.len()];

    for src in &discovered {
        let mut all_targets_can_skip = src.source_mtime > 0;

        if all_targets_can_skip {
            for tcfg in &cfg.targets {
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
                if !can_skip {
                    all_targets_can_skip = false;
                    break;
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

    // Parse and sanitize needed conversations in parallel, tracking drift/parse issues
    let job_results: Vec<ParseJobResult> = to_parse
        .par_iter()
        .map(|src| {
            let adapter = adapter_for(src.harness);
            match adapter.parse(src) {
                Ok(ParseOutcome::Conversation(mut conv)) => {
                    sanitize_conversation(&mut conv, cfg.redact_secrets);
                    if !conv.has_user_message() {
                        return ParseJobResult::Skipped;
                    }
                    let sha = conversation_sha256(&conv);
                    ParseJobResult::Parsed(ParsedConversation {
                        conversation: conv,
                        source_mtime: src.source_mtime,
                        source_size: src.source_size,
                        content_sha256: sha,
                    })
                }
                Ok(ParseOutcome::Skipped) => ParseJobResult::Skipped,
                Ok(ParseOutcome::UnrecognizedSchema(detail)) => {
                    ParseJobResult::Unrecognized(src.harness, detail)
                }
                Err(err) => ParseJobResult::Error(src.harness, err.to_string()),
            }
        })
        .collect();

    let mut parsed: Vec<ParsedConversation> = Vec::new();
    let mut unrecognized_by_harness: BTreeMap<Harness, (usize, String)> = BTreeMap::new();
    let mut errors_by_harness: BTreeMap<Harness, (usize, String)> = BTreeMap::new();

    for res in job_results {
        match res {
            ParseJobResult::Parsed(p) => parsed.push(p),
            ParseJobResult::Skipped => {}
            ParseJobResult::Unrecognized(h, detail) => {
                unrecognized_by_harness
                    .entry(h)
                    .and_modify(|(c, _)| *c += 1)
                    .or_insert((1, detail));
            }
            ParseJobResult::Error(h, detail) => {
                errors_by_harness
                    .entry(h)
                    .and_modify(|(c, _)| *c += 1)
                    .or_insert((1, detail));
            }
        }
    }

    for (h, (count, sample)) in unrecognized_by_harness {
        warnings.push(format!(
            "{}: {} file(s) had unrecognized format (e.g. {}) — harness schema may have changed",
            h.as_str(),
            count,
            sample
        ));
    }
    for (h, (count, sample)) in errors_by_harness {
        warnings.push(format!(
            "{}: {} session(s) failed to parse (e.g. {}) — harness format may have changed",
            h.as_str(),
            count,
            sample
        ));
    }

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
        let total_new: usize = reports.iter().map(|r| r.stats.new_count).sum();
        let total_updated: usize = reports.iter().map(|r| r.stats.updated_count).sum();
        let total_skipped: usize = reports.iter().map(|r| r.stats.skipped_count).sum();
        state.last_run = Some(LastRun {
            timestamp: Utc::now(),
            status: if warnings.is_empty() {
                RunStatus::Ok
            } else {
                RunStatus::Warning
            },
            new_count: total_new,
            updated_count: total_updated,
            skipped_count: total_skipped,
            warnings: warnings.clone(),
            error: None,
        });
        state.save_to(state_path)?;
    }

    Ok(RunOutcome { reports, warnings })
}

/// Persist a failed run's error into `state.json` so `agentbackup status` surfaces it.
pub fn record_run_error(state_path: &Path, err_msg: &str) -> Result<()> {
    let mut state = BackupState::load_or_default(state_path)?;
    state.last_run = Some(LastRun {
        timestamp: Utc::now(),
        status: RunStatus::Error,
        new_count: 0,
        updated_count: 0,
        skipped_count: 0,
        warnings: Vec::new(),
        error: Some(err_msg.to_string()),
    });
    state.save_to(state_path)
}

/// Send rate-limited (once per 24h per issue key) macOS notifications on run errors or drift warnings.
pub fn notify_issues_if_needed(
    cfg: &Config,
    state_path: &Path,
    warnings: &[String],
    error: Option<&str>,
) {
    if !cfg.notify {
        return;
    }
    let Ok(mut state) = BackupState::load_or_default(state_path) else {
        return;
    };
    let now = Utc::now();
    let mut state_dirty = false;

    if let Some(err) = error {
        let key = format!("error:{}", err.chars().take(80).collect::<String>());
        if state.check_and_mark_notification(&key, now, NOTIFICATION_COOLDOWN_HOURS) {
            state_dirty = true;
            notify::send_notification("Backup failed", err, notify::NotifySeverity::Error);
        }
    } else if !warnings.is_empty() {
        let mut to_alert = Vec::new();
        for w in warnings {
            let prefix = w.split(':').next().unwrap_or("drift");
            let key = format!("warning:{}", prefix);
            if state.check_and_mark_notification(&key, now, NOTIFICATION_COOLDOWN_HOURS) {
                state_dirty = true;
                to_alert.push(w.as_str());
            }
        }
        if !to_alert.is_empty() {
            let body = if to_alert.len() == 1 {
                to_alert[0].to_string()
            } else {
                format!(
                    "{} warnings detected (run `agentbackup status` for details)",
                    to_alert.len()
                )
            };
            notify::send_notification(
                "Schema drift or parse warning",
                &body,
                notify::NotifySeverity::Warning,
            );
        }
    }

    if state_dirty {
        let _ = state.save_to(state_path);
    }
}

/// Build the unified `agentbackup status` report (config, target health, last run, tracked counts, launchd schedule).
pub fn format_status_report(
    config_path: &Path,
    state_path: &Path,
    sched: &schedule::ScheduleInfo,
) -> Result<String> {
    use std::fmt::Write as _;

    let mut out = String::new();

    if !config_path.exists() {
        writeln!(
            out,
            "Config:   {} (not initialized — run `agentbackup init`)",
            contract_tilde(config_path)
        )?;
    } else {
        let cfg = Config::load_from(config_path)?;
        let state = BackupState::load_or_default(state_path)?;

        writeln!(out, "Config:   {} (ok)", contract_tilde(config_path))?;
        writeln!(
            out,
            "Notify:   {}",
            if cfg.notify {
                "enabled (desktop alerts on errors & drift warnings)"
            } else {
                "disabled"
            }
        )?;

        writeln!(out, "\nTargets:")?;
        if cfg.targets.is_empty() {
            writeln!(out, "  (none configured)")?;
        } else {
            for t in &cfg.targets {
                let resolved = t.resolved_path();
                let health = if resolved.is_dir() { "ok" } else { "MISSING" };
                let tracked = state
                    .targets
                    .get(&t.name)
                    .map(|ts| ts.conversations.len())
                    .unwrap_or(0);
                writeln!(
                    out,
                    "  {:<10} [{:<7}] {:<8} {} ({} tracked)",
                    t.name, health, t.format, t.path, tracked
                )?;
            }
        }

        writeln!(out, "\nLast Run:")?;
        match &state.last_run {
            Some(last) => {
                writeln!(
                    out,
                    "  Time:     {}",
                    last.timestamp.format("%Y-%m-%d %H:%M:%S UTC")
                )?;
                writeln!(out, "  Status:   {}", last.status.as_str())?;
                if last.status != RunStatus::Error {
                    writeln!(
                        out,
                        "  Summary:  new: {}, updated: {}, skipped: {}",
                        last.new_count, last.updated_count, last.skipped_count
                    )?;
                }
                if let Some(ref err) = last.error {
                    writeln!(out, "  Error:    {}", err)?;
                }
                if !last.warnings.is_empty() {
                    writeln!(out, "  Warnings:")?;
                    for w in &last.warnings {
                        writeln!(out, "    - {}", w)?;
                    }
                }
            }
            None => {
                writeln!(out, "  (no completed backup runs recorded yet)")?;
            }
        }

        // Per-harness tracked breakdown from the primary target (or union across targets)
        let mut by_harness: BTreeMap<Harness, usize> = BTreeMap::new();
        if let Some(first_target) = cfg.targets.first() {
            if let Some(ts) = state.targets.get(&first_target.name) {
                for rec in ts.conversations.values() {
                    *by_harness.entry(rec.harness).or_insert(0) += 1;
                }
            }
        }
        if !by_harness.is_empty() {
            let parts: Vec<String> = Harness::all()
                .iter()
                .map(|h| {
                    format!(
                        "{}: {}",
                        h.as_str(),
                        by_harness.get(h).copied().unwrap_or(0)
                    )
                })
                .collect();
            writeln!(out, "  Tracked:  {}", parts.join(", "))?;
        }
    }

    writeln!(out, "\nSchedule ({}):", sched.backend)?;
    if !sched.supported {
        writeln!(
            out,
            "  background scheduling is supported on macOS and Linux."
        )?;
    } else if !sched.installed {
        writeln!(
            out,
            "  not installed (run `agentbackup schedule install --every 1h`)"
        )?;
    } else {
        let interval_str = sched
            .interval_seconds
            .map(|s| format!("every {}s", s))
            .unwrap_or_else(|| "unknown interval".to_string());
        writeln!(
            out,
            "  Status:   installed ({}, active: {})",
            interval_str,
            if sched.loaded { "yes" } else { "no" }
        )?;
        if let Some(ref cfg_file) = sched.config_path {
            writeln!(out, "  Unit:     {}", contract_tilde(cfg_file))?;
        }
        writeln!(out, "  Log:      {}", contract_tilde(&sched.log_path))?;
        if !sched.recent_log_lines.is_empty() {
            writeln!(out, "  Recent log:")?;
            for line in &sched.recent_log_lines {
                writeln!(out, "    {}", line)?;
            }
        }
    }

    Ok(out)
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
        write!(writer, "Local backup directory path (must already exist): ")?;
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
        write!(
            writer,
            "Export format [markdown/txt/json] (default: markdown): "
        )?;
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
