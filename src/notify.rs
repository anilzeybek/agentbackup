use std::env;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifySeverity {
    Warning,
    Error,
}

fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
        .replace('\r', "")
}

pub fn build_osascript_notification(title: &str, subtitle: &str, body: &str) -> String {
    format!(
        "display notification \"{}\" with title \"{}\" subtitle \"{}\"",
        applescript_escape(body),
        applescript_escape(title),
        applescript_escape(subtitle),
    )
}

pub fn build_linux_notify_args(
    subtitle: &str,
    body: &str,
    severity: NotifySeverity,
) -> Vec<String> {
    let urgency = match severity {
        NotifySeverity::Warning => "normal",
        NotifySeverity::Error => "critical",
    };
    let clean_body = body.replace('\r', "").replace('\n', " ");
    vec![
        "-a".to_string(),
        "agentbackup".to_string(),
        "-u".to_string(),
        urgency.to_string(),
        format!("agentbackup — {}", subtitle),
        clean_body,
    ]
}

/// Send a native desktop notification on macOS (`osascript`) or Linux (`notify-send`).
/// Gracefully no-ops on headless environments, missing binaries, unsupported OSes,
/// or when `AGENTBACKUP_NO_NOTIFY=1` is set.
pub fn send_notification(subtitle: &str, body: &str, severity: NotifySeverity) {
    if env::var("AGENTBACKUP_NO_NOTIFY").is_ok() {
        return;
    }

    #[cfg(target_os = "macos")]
    {
        let _ = severity;
        let script = build_osascript_notification("agentbackup", subtitle, body);
        let _ = Command::new("osascript").args(["-e", &script]).output();
    }

    #[cfg(target_os = "linux")]
    {
        let args = build_linux_notify_args(subtitle, body, severity);
        let _ = Command::new("notify-send").args(&args).output();
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (subtitle, body, severity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_applescript_notification_safely() {
        let script = build_osascript_notification(
            "agentbackup",
            "Backup warning",
            "Target \"local\" failed\nline 2",
        );
        assert_eq!(
            script,
            "display notification \"Target \\\"local\\\" failed line 2\" with title \"agentbackup\" subtitle \"Backup warning\""
        );
    }

    #[test]
    fn builds_linux_notify_send_args() {
        let warn_args =
            build_linux_notify_args("Schema drift", "line1\nline2", NotifySeverity::Warning);
        assert_eq!(
            warn_args,
            vec![
                "-a",
                "agentbackup",
                "-u",
                "normal",
                "agentbackup — Schema drift",
                "line1 line2"
            ]
        );

        let err_args = build_linux_notify_args("Backup failed", "missing", NotifySeverity::Error);
        assert_eq!(err_args[3], "critical");
    }
}
