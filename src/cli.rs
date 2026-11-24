use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "agentbackup",
    version,
    about = "Cross-harness AI coding agent conversation backup CLI (Antigravity, Claude Code, Codex CLI, OpenCode)"
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

        /// Re-export all conversations even if unchanged
        #[arg(long)]
        force: bool,
    },

    /// Manage optional macOS launchd background backup schedule
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum ScheduleAction {
    /// Install a macOS launchd LaunchAgent to run backups automatically
    Install {
        /// Backup frequency (e.g. 30m, 1h, 6h, 1d)
        #[arg(long, default_value = "1h")]
        every: String,
    },

    /// Show current schedule status and recent log output
    Status,

    /// Remove the installed macOS launchd schedule
    Remove,
}
