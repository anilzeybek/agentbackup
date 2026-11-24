use anyhow::{bail, Context, Result};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::config::{config_dir, home_dir};

pub const LAUNCHD_LABEL: &str = "com.agentbackup.sync";

pub fn plist_path() -> PathBuf {
    home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", LAUNCHD_LABEL))
}

pub fn log_path() -> PathBuf {
    config_dir().join("launchd.log")
}

#[derive(Debug, Clone)]
pub struct ScheduleInfo {
    pub supported: bool,
    pub installed: bool,
    pub loaded: bool,
    pub interval_seconds: Option<u64>,
    pub plist_path: PathBuf,
    pub log_path: PathBuf,
    pub recent_log_lines: Vec<String>,
}

/// Inspect macOS launchd schedule state and recent log tail for `agentbackup status`.
pub fn inspect() -> ScheduleInfo {
    let plist = plist_path();
    let log = log_path();

    if !cfg!(target_os = "macos") {
        return ScheduleInfo {
            supported: false,
            installed: false,
            loaded: false,
            interval_seconds: None,
            plist_path: plist,
            log_path: log,
            recent_log_lines: Vec::new(),
        };
    }

    let installed = plist.exists();
    let interval_seconds = if installed {
        fs::read_to_string(&plist)
            .ok()
            .and_then(|xml| extract_plist_interval(&xml))
    } else {
        None
    };

    let loaded = if installed {
        Command::new("launchctl")
            .args(["list", LAUNCHD_LABEL])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        false
    };

    let recent_log_lines = if log.is_file() {
        fs::read_to_string(&log)
            .ok()
            .map(|raw| {
                let lines: Vec<&str> = raw.lines().collect();
                let start = lines.len().saturating_sub(8);
                lines[start..].iter().map(|s| s.to_string()).collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    ScheduleInfo {
        supported: true,
        installed,
        loaded,
        interval_seconds,
        plist_path: plist,
        log_path: log,
        recent_log_lines,
    }
}

/// Parse a human-friendly interval string like `30m`, `1h`, `6h`, `1d`, or `300s` into seconds.
pub fn parse_interval_seconds(spec: &str) -> Result<u64> {
    let s = spec.trim().to_lowercase();
    if s.is_empty() {
        bail!("Interval cannot be empty (examples: 30m, 1h, 6h, 1d)");
    }

    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60u64)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600u64)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86400u64)
    } else {
        bail!(
            "Invalid interval '{}'. Use a suffix like s, m, h, or d (examples: 30m, 1h, 1d).",
            spec
        );
    };

    let value: u64 = num_str
        .parse()
        .with_context(|| format!("Invalid numeric value in interval '{}'", spec))?;
    let total = value
        .checked_mul(multiplier)
        .context("Interval value overflowed")?;
    if total < 60 {
        bail!("Interval must be at least 60 seconds (e.g. 1m, 15m, 1h).");
    }
    Ok(total)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn build_plist_xml(exe_path: &str, log_file: &str, interval_seconds: u64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>run</string>
    </array>
    <key>StartInterval</key>
    <integer>{interval}</integer>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = LAUNCHD_LABEL,
        exe = xml_escape(exe_path),
        interval = interval_seconds,
        log = xml_escape(log_file),
    )
}

pub fn install(every: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("`agentbackup schedule` currently supports macOS launchd only.");
    }

    let seconds = parse_interval_seconds(every)?;
    let exe = env::current_exe().context("Failed to locate current agentbackup executable")?;
    let plist = plist_path();
    let log = log_path();

    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    if plist.exists() {
        let _ = unload_launchd(&plist);
    }

    let xml = build_plist_xml(&exe.display().to_string(), &log.display().to_string(), seconds);
    fs::write(&plist, xml)
        .with_context(|| format!("Failed to write LaunchAgent plist to {}", plist.display()))?;

    load_launchd(&plist)?;

    println!("Installed macOS launchd schedule ({})", LAUNCHD_LABEL);
    println!("  Interval: every {} ({}s)", every, seconds);
    println!("  Binary:   {}", exe.display());
    println!("  Plist:    {}", plist.display());
    println!("  Log:      {}", log.display());
    println!("Check health anytime with `agentbackup status`.");
    Ok(())
}

pub fn remove() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("`agentbackup schedule` currently supports macOS launchd only.");
    }

    let plist = plist_path();
    if !plist.exists() {
        println!("No agentbackup schedule is currently installed.");
        return Ok(());
    }

    let _ = unload_launchd(&plist);
    fs::remove_file(&plist)
        .with_context(|| format!("Failed to remove plist {}", plist.display()))?;
    println!("Removed macOS launchd schedule ({})", plist.display());
    Ok(())
}

fn current_uid() -> Option<String> {
    let out = Command::new("id").arg("-u").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn load_launchd(plist: &PathBuf) -> Result<()> {
    if let Some(uid) = current_uid() {
        let domain = format!("gui/{}", uid);
        let status = Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(plist)
            .status();
        if let Ok(s) = status {
            if s.success() {
                return Ok(());
            }
        }
    }

    let status = Command::new("launchctl")
        .args(["load", "-w"])
        .arg(plist)
        .status()
        .context("Failed to execute launchctl")?;
    if !status.success() {
        bail!("launchctl failed to load {}", plist.display());
    }
    Ok(())
}

fn unload_launchd(plist: &PathBuf) -> Result<()> {
    if let Some(uid) = current_uid() {
        let domain = format!("gui/{}", uid);
        let status = Command::new("launchctl")
            .args(["bootout", &domain])
            .arg(plist)
            .status();
        if let Ok(s) = status {
            if s.success() {
                return Ok(());
            }
        }
    }

    let _ = Command::new("launchctl")
        .args(["unload", "-w"])
        .arg(plist)
        .status();
    Ok(())
}

fn extract_plist_interval(xml: &str) -> Option<u64> {
    let key_idx = xml.find("<key>StartInterval</key>")?;
    let after_key = &xml[key_idx..];
    let int_start = after_key.find("<integer>")? + "<integer>".len();
    let rest = &after_key[int_start..];
    let int_end = rest.find("</integer>")?;
    rest[..int_end].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_intervals() {
        assert_eq!(parse_interval_seconds("60s").unwrap(), 60);
        assert_eq!(parse_interval_seconds("30m").unwrap(), 1800);
        assert_eq!(parse_interval_seconds("1h").unwrap(), 3600);
        assert_eq!(parse_interval_seconds("1d").unwrap(), 86400);
    }

    #[test]
    fn rejects_invalid_or_too_short_intervals() {
        assert!(parse_interval_seconds("10s").is_err());
        assert!(parse_interval_seconds("abc").is_err());
        assert!(parse_interval_seconds("").is_err());
    }

    #[test]
    fn builds_and_parses_plist() {
        let xml = build_plist_xml("/usr/local/bin/agentbackup", "/tmp/log.txt", 3600);
        assert!(xml.contains("<string>com.agentbackup.sync</string>"));
        assert!(xml.contains("<string>/usr/local/bin/agentbackup</string>"));
        assert_eq!(extract_plist_interval(&xml), Some(3600));
    }
}
