use std::env;
use std::process::Command;

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

/// Send a native macOS notification via `osascript` (no-op on non-macOS or when
/// `AGENTBACKUP_NO_NOTIFY=1` is set). Failures are ignored so notifications never crash a backup.
pub fn send_notification(subtitle: &str, body: &str) {
    if !cfg!(target_os = "macos") {
        return;
    }
    if env::var("AGENTBACKUP_NO_NOTIFY").is_ok() {
        return;
    }

    let script = build_osascript_notification("agentbackup", subtitle, body);
    let _ = Command::new("osascript").args(["-e", &script]).output();
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
}
