use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "agentbackup",
    version,
    about = "Cross-harness AI coding agent conversation backup CLI (Antigravity, Claude Code, Codex CLI, OpenCode, Pi, DeepSeek Harness)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Detect installed harnesses and configure your local backup destination
    Init,

    /// Read-only inventory of detected harnesses, session counts, and raw vs estimated export size
    Scan,

    /// Run an incremental backup to your configured destination(s)
    Run {
        /// Preview what would be written without modifying target files or state
        #[arg(long)]
        dry_run: bool,
    },

    /// Show backup health summary, target checks, drift warnings, and background schedule status
    Status,

    /// Manage optional background backup schedule (macOS launchd / Linux systemd or cron)
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum ScheduleAction {
    /// Install a background schedule to run backups automatically
    Install {
        /// Backup frequency (e.g. 30m, 1h, 6h, 1d)
        #[arg(long, default_value = "1h")]
        every: String,
    },

    /// Remove the installed background schedule
    Remove,
}
