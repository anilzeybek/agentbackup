use anyhow::{bail, Result};
use clap::Parser;
use std::io;

use agentbackup::cli::{Cli, Commands, ScheduleAction};
use agentbackup::config::{default_config_path, default_state_path, Config};
use agentbackup::{format_bytes, init_config_interactive, run_backup, scan_harnesses, schedule};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = default_config_path();
    let state_path = default_state_path();

    match cli.command {
        Commands::Init => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            let mut reader = stdin.lock();
            let mut writer = stdout.lock();
            init_config_interactive(&config_path, &mut reader, &mut writer)?;
        }

        Commands::Scan => {
            let cfg = if config_path.exists() {
                Config::load_from(&config_path)?
            } else {
                Config::default()
            };

            let summaries = scan_harnesses(&cfg)?;

            println!(
                "{:<12} {:>8} {:>10} {:>12}   {}",
                "Harness", "Sessions", "Raw Size", "Est. Export", "Source"
            );
            println!("{}", "-".repeat(78));

            let mut total_sessions = 0usize;
            let mut total_raw = 0u64;
            let mut total_est = 0u64;

            for s in &summaries {
                total_sessions += s.session_count;
                total_raw += s.raw_bytes;
                total_est += s.est_export_bytes;

                let src_label = if s.exists {
                    s.display_path.clone()
                } else {
                    format!("{} (not found)", s.display_path)
                };

                println!(
                    "{:<12} {:>8} {:>10} {:>12}   {}",
                    s.harness.as_str(),
                    s.session_count,
                    format_bytes(s.raw_bytes),
                    format_bytes(s.est_export_bytes),
                    src_label
                );
            }

            println!("{}", "-".repeat(78));
            println!(
                "{:<12} {:>8} {:>10} {:>12}",
                "Total",
                total_sessions,
                format_bytes(total_raw),
                format_bytes(total_est)
            );
        }

        Commands::Run { dry_run } => {
            if !config_path.exists() {
                bail!(
                    "Config file not found at {}. Run `agentbackup init` first.",
                    config_path.display()
                );
            }
            let cfg = Config::load_from(&config_path)?;
            let reports = run_backup(&cfg, &state_path, dry_run)?;

            if dry_run {
                println!("[dry-run] No files or state were modified.\n");
            }

            for report in reports {
                println!(
                    "Target '{}' ({}, {}):",
                    report.name, report.format, report.path
                );
                println!(
                    "  new: {}, updated: {}, skipped: {}, tracked: {} (wrote {} files, {})",
                    report.stats.new_count,
                    report.stats.updated_count,
                    report.stats.skipped_count,
                    report.stats.total_tracked,
                    report.stats.files_written,
                    format_bytes(report.stats.bytes_written)
                );
            }
        }

        Commands::Schedule { action } => match action {
            ScheduleAction::Install { every } => schedule::install(&every)?,
            ScheduleAction::Status => schedule::status()?,
            ScheduleAction::Remove => schedule::remove()?,
        },
    }

    Ok(())
}
