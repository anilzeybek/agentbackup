use anyhow::{bail, Context, Result};
use std::env;
use std::fs;
#[cfg(target_os = "linux")]
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Stdio;

use crate::config::{config_dir, home_dir};

pub const LAUNCHD_LABEL: &str = "com.agentbackup.sync";
pub const SYSTEMD_UNIT: &str = "agentbackup";
pub const CRON_MARKER: &str = "# agentbackup-schedule";

pub fn plist_path() -> PathBuf {
    home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", LAUNCHD_LABEL))
}

pub fn systemd_user_dir() -> PathBuf {
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("systemd").join("user");
        }
    }
    home_dir().join(".config").join("systemd").join("user")
}

pub fn systemd_service_path() -> PathBuf {
    systemd_user_dir().join(format!("{}.service", SYSTEMD_UNIT))
}

pub fn systemd_timer_path() -> PathBuf {
    systemd_user_dir().join(format!("{}.timer", SYSTEMD_UNIT))
}

pub fn log_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        config_dir().join("launchd.log")
    } else {
        config_dir().join("schedule.log")
    }
}

#[derive(Debug, Clone)]
pub struct ScheduleInfo {
    pub backend: String,
    pub supported: bool,
    pub installed: bool,
    pub loaded: bool,
    pub interval_seconds: Option<u64>,
    pub config_path: Option<PathBuf>,
    pub log_path: PathBuf,
    pub recent_log_lines: Vec<String>,
}

fn read_recent_log_lines(log: &PathBuf) -> Vec<String> {
    if !log.is_file() {
        return Vec::new();
    }
    fs::read_to_string(log)
        .ok()
        .map(|raw| {
            let lines: Vec<&str> = raw.lines().collect();
            let start = lines.len().saturating_sub(8);
            lines[start..].iter().map(|s| s.to_string()).collect()
        })
        .unwrap_or_default()
}

/// Inspect background schedule state (macOS launchd or Linux systemd --user / cron).
pub fn inspect() -> ScheduleInfo {
    let log = log_path();
    let recent_log_lines = read_recent_log_lines(&log);

    #[cfg(target_os = "macos")]
    {
        let plist = plist_path();
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

        return ScheduleInfo {
            backend: "macOS launchd".to_string(),
            supported: true,
            installed,
            loaded,
            interval_seconds,
            config_path: Some(plist),
            log_path: log,
            recent_log_lines,
        };
    }

    #[cfg(target_os = "linux")]
    {
        let timer = systemd_timer_path();
        if timer.exists() {
            let interval_seconds = fs::read_to_string(&timer)
                .ok()
                .and_then(|txt| extract_systemd_interval(&txt));
            let loaded = Command::new("systemctl")
                .args(["--user", "is-active", "--quiet", &format!("{}.timer", SYSTEMD_UNIT)])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            return ScheduleInfo {
                backend: "Linux systemd --user".to_string(),
                supported: true,
                installed: true,
                loaded,
                interval_seconds,
                config_path: Some(timer),
                log_path: log,
                recent_log_lines,
            };
        }

        // Check crontab fallback
        if let Some(cron_line) = read_existing_cron_line() {
            let interval_seconds = extract_cron_interval_seconds(&cron_line);
            return ScheduleInfo {
                backend: "Linux cron".to_string(),
                supported: true,
                installed: true,
                loaded: true,
                interval_seconds,
                config_path: None,
                log_path: log,
                recent_log_lines,
            };
        }

        let backend = if systemd_user_available() {
            "Linux systemd --user"
        } else {
            "Linux cron"
        };
        return ScheduleInfo {
            backend: backend.to_string(),
            supported: true,
            installed: false,
            loaded: false,
            interval_seconds: None,
            config_path: Some(timer),
            log_path: log,
            recent_log_lines,
        };
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        ScheduleInfo {
            backend: "unsupported".to_string(),
            supported: false,
            installed: false,
            loaded: false,
            interval_seconds: None,
            config_path: None,
            log_path: log,
            recent_log_lines,
        }
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

pub fn build_systemd_service(exe_path: &str, log_file: &str) -> String {
    format!(
        "[Unit]\n\
Description=agentbackup — Cross-harness AI conversation backup\n\n\
[Service]\n\
Type=oneshot\n\
ExecStart=\"{exe}\" run\n\
StandardOutput=append:{log}\n\
StandardError=append:{log}\n",
        exe = exe_path.replace('"', "\\\""),
        log = log_file,
    )
}

pub fn build_systemd_timer(interval_seconds: u64) -> String {
    format!(
        "[Unit]\n\
Description=Run agentbackup periodically\n\n\
[Timer]\n\
OnBootSec=2m\n\
OnUnitActiveSec={interval}s\n\
Persistent=true\n\n\
[Install]\n\
WantedBy=timers.target\n",
        interval = interval_seconds,
    )
}

pub fn cron_expr_from_seconds(seconds: u64) -> String {
    let mins = (seconds / 60).max(1);
    if mins < 60 {
        format!("*/{} * * * *", mins)
    } else if mins % 1440 == 0 {
        let days = (mins / 1440).clamp(1, 31);
        format!("0 0 */{} * *", days)
    } else {
        let hours = (mins / 60).clamp(1, 23);
        format!("0 */{} * * *", hours)
    }
}

pub fn build_cron_line(exe_path: &str, log_file: &str, interval_seconds: u64) -> String {
    let expr = cron_expr_from_seconds(interval_seconds);
    format!(
        "{} \"{}\" run >> \"{}\" 2>&1 {} interval={}s",
        expr, exe_path, log_file, CRON_MARKER, interval_seconds
    )
}

pub fn install(every: &str) -> Result<()> {
    let seconds = parse_interval_seconds(every)?;
    let exe = env::current_exe().context("Failed to locate current agentbackup executable")?;
    let log = log_path();

    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    #[cfg(target_os = "macos")]
    {
        return install_macos(&exe, &log, every, seconds);
    }

    #[cfg(target_os = "linux")]
    {
        return install_linux(&exe, &log, every, seconds);
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (seconds, exe, log);
        bail!("`agentbackup schedule` supports macOS (launchd) and Linux (systemd/cron).");
    }
}

pub fn remove() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        return remove_macos();
    }

    #[cfg(target_os = "linux")]
    {
        return remove_linux();
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        bail!("`agentbackup schedule` supports macOS (launchd) and Linux (systemd/cron).");
    }
}

#[cfg(target_os = "macos")]
fn install_macos(exe: &PathBuf, log: &PathBuf, every: &str, seconds: u64) -> Result<()> {
    let plist = plist_path();
    if let Some(parent) = plist.parent() {
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

#[cfg(target_os = "macos")]
fn remove_macos() -> Result<()> {
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "linux")]
fn systemd_user_available() -> bool {
    Command::new("systemctl")
        .args(["--user", "status"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.code().map(|c| c == 0 || c == 3).unwrap_or(false))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn install_linux(exe: &PathBuf, log: &PathBuf, every: &str, seconds: u64) -> Result<()> {
    if systemd_user_available() {
        let dir = systemd_user_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;

        let service_path = systemd_service_path();
        let timer_path = systemd_timer_path();

        fs::write(
            &service_path,
            build_systemd_service(&exe.display().to_string(), &log.display().to_string()),
        )
        .with_context(|| format!("Failed to write {}", service_path.display()))?;
        fs::write(&timer_path, build_systemd_timer(seconds))
            .with_context(|| format!("Failed to write {}", timer_path.display()))?;

        let reload = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()
            .context("Failed to execute systemctl --user daemon-reload")?;
        if !reload.success() {
            bail!("systemctl --user daemon-reload failed");
        }

        let timer_name = format!("{}.timer", SYSTEMD_UNIT);
        let enable = Command::new("systemctl")
            .args(["--user", "enable", "--now", &timer_name])
            .status()
            .context("Failed to execute systemctl --user enable --now")?;
        if !enable.success() {
            bail!("systemctl --user enable --now {} failed", timer_name);
        }

        // Remove any legacy cron entry so it doesn't double-run
        let _ = remove_cron_entry();

        println!("Installed Linux systemd --user schedule ({})", timer_name);
        println!("  Interval: every {} ({}s)", every, seconds);
        println!("  Binary:   {}", exe.display());
        println!("  Timer:    {}", timer_path.display());
        println!("  Service:  {}", service_path.display());
        println!("  Log:      {}", log.display());
        println!("Check health anytime with `agentbackup status`.");
        return Ok(());
    }

    // Fallback to user crontab when systemd --user is not active (e.g. WSL / minimal distros)
    let line = build_cron_line(&exe.display().to_string(), &log.display().to_string(), seconds);
    install_cron_entry(&line)?;
    println!("Installed Linux cron schedule (systemd --user not active; using crontab)");
    println!("  Interval: every {} ({}s)", every, seconds);
    println!("  Binary:   {}", exe.display());
    println!("  Log:      {}", log.display());
    println!("Check health anytime with `agentbackup status`.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_linux() -> Result<()> {
    let mut removed_any = false;
    let service_path = systemd_service_path();
    let timer_path = systemd_timer_path();

    if timer_path.exists() || service_path.exists() {
        let timer_name = format!("{}.timer", SYSTEMD_UNIT);
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "--now", &timer_name])
            .status();
        if timer_path.exists() {
            fs::remove_file(&timer_path)
                .with_context(|| format!("Failed to remove {}", timer_path.display()))?;
        }
        if service_path.exists() {
            fs::remove_file(&service_path)
                .with_context(|| format!("Failed to remove {}", service_path.display()))?;
        }
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        println!("Removed Linux systemd --user schedule ({})", timer_name);
        removed_any = true;
    }

    if remove_cron_entry()? {
        println!("Removed Linux cron schedule.");
        removed_any = true;
    }

    if !removed_any {
        println!("No agentbackup schedule is currently installed.");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_crontab() -> String {
    Command::new("crontab")
        .arg("-l")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn read_existing_cron_line() -> Option<String> {
    read_crontab()
        .lines()
        .find(|l| l.contains(CRON_MARKER))
        .map(|s| s.to_string())
}

#[cfg(target_os = "linux")]
fn write_crontab(content: &str) -> Result<()> {
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to execute `crontab -` (is cron installed?)")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("`crontab -` exited with an error");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_cron_entry(new_line: &str) -> Result<()> {
    let existing = read_crontab();
    let mut lines: Vec<&str> = existing
        .lines()
        .filter(|l| !l.contains(CRON_MARKER))
        .collect();
    lines.push(new_line);
    let mut body = lines.join("\n");
    body.push('\n');
    write_crontab(&body)
}

#[cfg(target_os = "linux")]
fn remove_cron_entry() -> Result<bool> {
    let existing = read_crontab();
    if !existing.lines().any(|l| l.contains(CRON_MARKER)) {
        return Ok(false);
    }
    let remaining: Vec<&str> = existing
        .lines()
        .filter(|l| !l.contains(CRON_MARKER))
        .collect();
    let mut body = remaining.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    write_crontab(&body)?;
    Ok(true)
}

pub fn extract_plist_interval(xml: &str) -> Option<u64> {
    let key_idx = xml.find("<key>StartInterval</key>")?;
    let after_key = &xml[key_idx..];
    let int_start = after_key.find("<integer>")? + "<integer>".len();
    let rest = &after_key[int_start..];
    let int_end = rest.find("</integer>")?;
    rest[..int_end].trim().parse().ok()
}

pub fn extract_systemd_interval(timer_unit: &str) -> Option<u64> {
    for line in timer_unit.lines() {
        if let Some(val) = line.trim().strip_prefix("OnUnitActiveSec=") {
            return parse_interval_seconds(val).ok();
        }
    }
    None
}

pub fn extract_cron_interval_seconds(cron_line: &str) -> Option<u64> {
    let idx = cron_line.find("interval=")?;
    let rest = &cron_line[idx + "interval=".len()..];
    let token = rest.split_whitespace().next()?;
    parse_interval_seconds(token).ok()
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
    fn builds_and_parses_macos_plist() {
        let xml = build_plist_xml("/usr/local/bin/agentbackup", "/tmp/log.txt", 3600);
        assert!(xml.contains("<string>com.agentbackup.sync</string>"));
        assert!(xml.contains("<string>/usr/local/bin/agentbackup</string>"));
        assert_eq!(extract_plist_interval(&xml), Some(3600));
    }

    #[test]
    fn builds_and_parses_linux_systemd_and_cron() {
        let svc = build_systemd_service("/home/dev/.local/bin/agentbackup", "/tmp/schedule.log");
        assert!(svc.contains("ExecStart=\"/home/dev/.local/bin/agentbackup\" run"));
        assert!(svc.contains("StandardOutput=append:/tmp/schedule.log"));

        let timer = build_systemd_timer(1800);
        assert!(timer.contains("OnUnitActiveSec=1800s"));
        assert_eq!(extract_systemd_interval(&timer), Some(1800));

        let cron = build_cron_line(
            "/home/dev/.local/bin/agentbackup",
            "/tmp/schedule.log",
            3600,
        );
        assert!(cron.starts_with("0 */1 * * * "));
        assert!(cron.contains(CRON_MARKER));
        assert_eq!(extract_cron_interval_seconds(&cron), Some(3600));
    }
}
