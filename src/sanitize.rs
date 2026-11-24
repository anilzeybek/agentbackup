use regex::Regex;
use std::sync::OnceLock;

use crate::models::{Conversation, Message, Role};

fn re_user_request() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)<USER_REQUEST>\s*(.*?)\s*</USER_REQUEST>").unwrap())
}

fn re_strip_blocks() -> &'static [Regex] {
    static RES: OnceLock<Vec<Regex>> = OnceLock::new();
    RES.get_or_init(|| {
        vec![
            Regex::new(r"(?s)<ADDITIONAL_METADATA>.*?</ADDITIONAL_METADATA>").unwrap(),
            Regex::new(r"(?s)<USER_SETTINGS_CHANGE>.*?</USER_SETTINGS_CHANGE>").unwrap(),
            Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").unwrap(),
            Regex::new(r"(?s)<environment_context>.*?</environment_context>").unwrap(),
            Regex::new(r"(?s)<ide_opened_file>.*?</ide_opened_file>").unwrap(),
            Regex::new(r"(?s)<ide_selection>.*?</ide_selection>").unwrap(),
        ]
    })
}

fn re_md_data_image() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"!\[[^\]]*\]\(data:image/[a-zA-Z0-9.+-]+;base64,[A-Za-z0-9+/=\r\n]+\)").unwrap()
    })
}

fn re_data_image() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"data:image/[a-zA-Z0-9.+-]+;base64,[A-Za-z0-9+/=]+").unwrap())
}

fn re_data_media() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"data:(?:audio|video)/[a-zA-Z0-9.+-]+;base64,[A-Za-z0-9+/=]+").unwrap()
    })
}

fn re_data_other() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"data:[a-zA-Z0-9.+-]+/[a-zA-Z0-9.+-]+;base64,[A-Za-z0-9+/=]+").unwrap()
    })
}

fn re_raw_base64_blob() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Match unbroken base64-like blobs of 512+ chars (likely embedded binary/image payload)
    RE.get_or_init(|| Regex::new(r"(?m)^[A-Za-z0-9+/]{512,}={0,2}$").unwrap())
}

fn re_secrets() -> &'static [(Regex, &'static str)] {
    static RES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    RES.get_or_init(|| {
        vec![
            (
                Regex::new(
                    r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
                )
                .unwrap(),
                "[REDACTED:PRIVATE_KEY]",
            ),
            (
                Regex::new(r"\bsk-ant-[A-Za-z0-9_-]{20,}\b").unwrap(),
                "[REDACTED:ANTHROPIC_KEY]",
            ),
            (
                Regex::new(r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{20,}\b").unwrap(),
                "[REDACTED:OPENAI_KEY]",
            ),
            (
                Regex::new(r"\bAIza[0-9A-Za-z_-]{35}\b").unwrap(),
                "[REDACTED:GOOGLE_API_KEY]",
            ),
            (
                Regex::new(
                    r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}\b|\bgithub_pat_[A-Za-z0-9_]{30,}\b",
                )
                .unwrap(),
                "[REDACTED:GITHUB_TOKEN]",
            ),
            (
                Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").unwrap(),
                "[REDACTED:AWS_ACCESS_KEY]",
            ),
            (
                Regex::new(r"\bxox[baprs]-[0-9A-Za-z-]{10,}\b").unwrap(),
                "[REDACTED:SLACK_TOKEN]",
            ),
            (
                Regex::new(r"(?i)\b(Bearer\s+)[A-Za-z0-9._~+/=-]{20,}").unwrap(),
                "${1}[REDACTED:BEARER_TOKEN]",
            ),
        ]
    })
}

/// Remove internal agent wrapper tags from a user prompt.
pub fn clean_user_prompt(raw: &str) -> String {
    if let Some(caps) = re_user_request().captures(raw) {
        if let Some(inner) = caps.get(1) {
            return inner.as_str().trim().to_string();
        }
    }

    let mut text = raw.to_string();
    for re in re_strip_blocks() {
        text = re.replace_all(&text, "").into_owned();
    }
    text.trim().to_string()
}

/// Strip base64 data URIs and large binary/base64 payloads, replacing them with placeholders.
pub fn strip_blobs(input: &str) -> String {
    let s = re_md_data_image().replace_all(input, "[image omitted]");
    let s = re_data_image().replace_all(&s, "[image omitted]");
    let s = re_data_media().replace_all(&s, "[media omitted]");
    let s = re_data_other().replace_all(&s, "[attachment omitted]");
    let s = re_raw_base64_blob().replace_all(&s, "[attachment omitted]");
    s.trim().to_string()
}

/// Redact common credential patterns.
pub fn redact_secrets(input: &str) -> String {
    let mut out = input.to_string();
    for (re, replacement) in re_secrets() {
        out = re.replace_all(&out, *replacement).into_owned();
    }
    out
}

/// Apply full hygiene pipeline to a conversation in place.
pub fn sanitize_conversation(conv: &mut Conversation, do_redact_secrets: bool) {
    let mut cleaned_messages = Vec::with_capacity(conv.messages.len());

    for msg in conv.messages.drain(..) {
        let mut text = match msg.role {
            Role::User => clean_user_prompt(&msg.content),
            Role::Assistant => msg.content.trim().to_string(),
        };
        text = strip_blobs(&text);
        if do_redact_secrets {
            text = redact_secrets(&text);
        }
        let text = text.trim().to_string();
        if !text.is_empty() {
            cleaned_messages.push(Message {
                role: msg.role,
                content: text,
                timestamp: msg.timestamp,
                model: msg.model,
            });
        }
    }

    conv.messages = cleaned_messages;
    conv.project = sanitize_project_name(&conv.project);

    if conv.title.trim().is_empty() {
        conv.title = derive_title(&conv.messages);
    } else {
        let mut title = clean_user_prompt(&conv.title);
        title = strip_blobs(&title);
        if do_redact_secrets {
            title = redact_secrets(&title);
        }
        title = collapse_whitespace(&title);
        if title.is_empty() {
            title = derive_title(&conv.messages);
        }
        conv.title = truncate_chars(&title, 80);
    }

    conv.refresh_derived_fields();
}

pub fn derive_title(messages: &[Message]) -> String {
    for msg in messages {
        if msg.role == Role::User {
            for line in msg.content.lines() {
                let cleaned = collapse_whitespace(line);
                if cleaned.is_empty()
                    || cleaned == "[image omitted]"
                    || cleaned == "[attachment omitted]"
                    || cleaned == "[media omitted]"
                {
                    continue;
                }
                return truncate_chars(&cleaned, 72);
            }
        }
    }
    "Untitled conversation".to_string()
}

pub fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", truncated.trim_end())
}

pub fn slugify(input: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut prev_dash = true;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= max_len {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    }
}

pub fn sanitize_project_name(input: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in input.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let cleaned = out.trim_matches(|c| c == '-' || c == '.').to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "general".to_string()
    } else {
        cleaned
    }
}

pub const WORKSPACE_CONTAINERS: &[&str] = &[
    "workdir",
    "projects",
    "project",
    "code",
    "dev",
    "repos",
    "repo",
    "src",
    "workspace",
    "workspaces",
    "git",
    "github",
    "work",
    "Documents",
    "Desktop",
];

pub const NON_PROJECT_HOME_DIRS: &[&str] = &[
    "Library",
    "Applications",
    "Downloads",
    "Movies",
    "Music",
    "Pictures",
    "Public",
];

const SYSTEM_PREFIXES: &[&str] = &[
    "/System",
    "/Library",
    "/Applications",
    "/usr",
    "/etc",
    "/opt",
    "/dev",
    "/proc",
    "/sys",
    "/bin",
    "/sbin",
];

const TEMP_PREFIXES: &[&str] = &["/tmp", "/private/tmp", "/var", "/private/var"];

fn path_has_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .map(|rest| rest.starts_with('/'))
            .unwrap_or(false)
}

/// Resolve the Git repository name (handling both normal `.git/` repos and `.git` worktree files)
/// by walking upward from `path` without crossing `~`, temp roots, or `/`.
pub fn git_repo_name_for_path(path: &std::path::Path) -> Option<String> {
    let mut cur = if path.is_file() {
        path.parent()?
    } else if path.is_dir() {
        path
    } else {
        return None;
    };

    let home = dirs::home_dir();

    loop {
        // Stop before crossing home directory, system roots, or `/`
        if cur.parent().is_none() {
            break;
        }
        if let Some(ref h) = home {
            if cur == h.as_path() {
                break;
            }
        }
        let cur_str = cur.to_string_lossy();
        if cur_str == "/Users"
            || cur_str == "/home"
            || cur_str == "/tmp"
            || cur_str == "/private/tmp"
            || cur_str == "/var"
            || cur_str == "/private/var"
        {
            break;
        }
        // Also stop at `/Users/<user>` or `/home/<user>` even if different from current user
        let parts: Vec<&str> = cur_str.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() == 2 && (parts[0] == "Users" || parts[0] == "home") {
            break;
        }

        let dot_git = cur.join(".git");
        if dot_git.is_dir() {
            let name = cur.file_name()?.to_str()?;
            let sanitized = sanitize_project_name(name);
            return if sanitized == "general" {
                None
            } else {
                Some(sanitized)
            };
        }
        if dot_git.is_file() {
            if let Ok(content) = std::fs::read_to_string(&dot_git) {
                if let Some(gitdir_line) = content
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("gitdir:"))
                {
                    let raw_gitdir = gitdir_line.trim();
                    let gitdir_path = if std::path::Path::new(raw_gitdir).is_absolute() {
                        std::path::PathBuf::from(raw_gitdir)
                    } else {
                        cur.join(raw_gitdir)
                    };
                    // Worktree pointer: `<repo>/.git/worktrees/<name>` -> `<repo>`
                    let components: Vec<&std::ffi::OsStr> =
                        gitdir_path.iter().collect();
                    if let Some(git_idx) = components.iter().position(|c| *c == ".git") {
                        if git_idx > 0 {
                            let repo_name = components[git_idx - 1].to_string_lossy();
                            let sanitized = sanitize_project_name(&repo_name);
                            if sanitized != "general" {
                                return Some(sanitized);
                            }
                        }
                    }
                }
            }
            // Fallback for submodule `.git` file: use directory containing `.git`
            let name = cur.file_name()?.to_str()?;
            let sanitized = sanitize_project_name(name);
            return if sanitized == "general" {
                None
            } else {
                Some(sanitized)
            };
        }

        cur = cur.parent()?;
    }

    None
}

/// Extract a normalized project name from a filesystem path string.
/// - Non-project paths (`~`, `/`, `/tmp`, `/var`, `~/Library`, `~/Desktop`, `~/workdir`, `~/.claude`, etc.) return `None` (caller uses `"general"`).
/// - Nested paths inside a Git repository or Git worktree resolve to the Git repo root name.
/// - Deleted/non-existent paths under known workspace containers (`~/workdir/<project>/sub/dir`) resolve to `<project>`.
pub fn project_name_from_path(path_str: &str) -> Option<String> {
    let trimmed = path_str.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" || trimmed == "~" {
        return None;
    }

    // Reject OS system directories immediately
    for prefix in SYSTEM_PREFIXES {
        if path_has_prefix(trimmed, prefix) {
            return None;
        }
    }

    let parts: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }

    // Inspect home-relative structure (`/Users/<user>/...` or `/home/<user>/...`)
    let is_home_path = parts.len() >= 2 && (parts[0] == "Users" || parts[0] == "home");
    if is_home_path {
        // Home directory itself (`~`) -> general
        if parts.len() == 2 {
            return None;
        }
        let dir1 = parts[2];
        // Hidden dot-dirs (`.claude`, `.gemini`, `.codex`, `.config`, ...) or non-project home dirs -> general
        if dir1.starts_with('.') || NON_PROJECT_HOME_DIRS.contains(&dir1) {
            return None;
        }
        // Container dir itself (`~/workdir`, `~/Desktop`, `~/Documents`, ...) -> general
        if parts.len() == 3 && WORKSPACE_CONTAINERS.contains(&dir1) {
            return None;
        }
    }

    let path = std::path::Path::new(trimmed);

    // If path exists on disk, prefer Git repo / worktree root name
    if path.exists() {
        if let Some(repo) = git_repo_name_for_path(path) {
            return Some(repo);
        }
    }

    // Temp directories without a Git repo -> general
    for prefix in TEMP_PREFIXES {
        if path_has_prefix(trimmed, prefix) {
            return None;
        }
    }

    // Fallback for non-Git or deleted paths under a workspace container (`~/workdir/<project>/sub/dir`)
    if is_home_path && WORKSPACE_CONTAINERS.contains(&parts[2]) && parts.len() >= 4 {
        let proj = parts[3];
        if proj.starts_with('.') {
            return None;
        }
        let sanitized = sanitize_project_name(proj);
        return if sanitized == "general" {
            None
        } else {
            Some(sanitized)
        };
    }

    let name = path.file_name()?.to_str()?;
    if name.starts_with('.') {
        return None;
    }
    let sanitized = sanitize_project_name(name);
    if sanitized == "general" {
        None
    } else {
        Some(sanitized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_antigravity_user_request_wrappers() {
        let raw = "<USER_REQUEST>\nHello world\nSecond line\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-24T16:34:52Z.\n</ADDITIONAL_METADATA>\n<USER_SETTINGS_CHANGE>\nChanged model.\n</USER_SETTINGS_CHANGE>";
        assert_eq!(clean_user_prompt(raw), "Hello world\nSecond line");
    }

    #[test]
    fn strips_base64_images_and_blobs() {
        let input = "Look at this ![pic](data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==) and data:application/pdf;base64,JVBERi0xLjQ= done";
        let out = strip_blobs(input);
        assert_eq!(
            out,
            "Look at this [image omitted] and [attachment omitted] done"
        );
    }

    #[test]
    fn redacts_common_secrets() {
        let input = "keys: sk-ant-api03-1234567890abcdefghijklmno sk-proj-1234567890abcdefghijklmno AIzaSyA12345678901234567890123456789012 ghp_123456789012345678901234567890123456 AKIAIOSFODNN7EXAMPLE Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.abc";
        let out = redact_secrets(input);
        assert!(out.contains("[REDACTED:ANTHROPIC_KEY]"));
        assert!(out.contains("[REDACTED:OPENAI_KEY]"));
        assert!(out.contains("[REDACTED:GOOGLE_API_KEY]"));
        assert!(out.contains("[REDACTED:GITHUB_TOKEN]"));
        assert!(out.contains("[REDACTED:AWS_ACCESS_KEY]"));
        assert!(out.contains("Bearer [REDACTED:BEARER_TOKEN]"));
        assert!(!out.contains("sk-ant-"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redacts_pem_private_key() {
        let input = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\nafter";
        let out = redact_secrets(input);
        assert_eq!(out, "before\n[REDACTED:PRIVATE_KEY]\nafter");
    }

    #[test]
    fn maps_non_project_dirs_to_none_general() {
        assert_eq!(project_name_from_path("/"), None);
        assert_eq!(project_name_from_path("~"), None);
        assert_eq!(project_name_from_path("/Users/anilzeybek"), None);
        assert_eq!(project_name_from_path("/Users/anilzeybek/"), None);
        assert_eq!(project_name_from_path("/Users/anilzeybek/workdir"), None);
        assert_eq!(project_name_from_path("/Users/anilzeybek/Desktop"), None);
        assert_eq!(project_name_from_path("/Users/anilzeybek/Downloads/foo"), None);
        assert_eq!(
            project_name_from_path(
                "/Users/anilzeybek/Library/Application Support/Claude/2026-11-07-ephemeral"
            ),
            None
        );
        assert_eq!(project_name_from_path("/Users/anilzeybek/.claude/projects"), None);
        assert_eq!(project_name_from_path("/tmp"), None);
        assert_eq!(project_name_from_path("/tmp/scratch-session"), None);
        assert_eq!(project_name_from_path("/private/tmp/scratch-session"), None);
    }

    #[test]
    fn resolves_nested_paths_via_git_repo_worktree_and_container_fallback() {
        let tmp = tempfile::TempDir::new().unwrap();

        // 1. Normal Git repo with nested subdirectory
        let repo_root = tmp.path().join("my-rust-repo");
        let nested = repo_root.join("crates").join("core").join("src");
        std::fs::create_dir_all(repo_root.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            project_name_from_path(nested.to_str().unwrap()),
            Some("my-rust-repo".to_string())
        );

        // 2. Git worktree pointing back to main repo
        let wt_dir = tmp.path().join("ephemeral-wt-abc123");
        let wt_sub = wt_dir.join("frontend");
        std::fs::create_dir_all(&wt_sub).unwrap();
        let main_gitdir = repo_root
            .join(".git")
            .join("worktrees")
            .join("ephemeral-wt-abc123");
        std::fs::write(
            wt_dir.join(".git"),
            format!("gitdir: {}\n", main_gitdir.display()),
        )
        .unwrap();
        assert_eq!(
            project_name_from_path(wt_sub.to_str().unwrap()),
            Some("my-rust-repo".to_string())
        );

        // 3. Non-Git temp dir maps to None (general)
        let plain_tmp = tmp.path().join("just-a-scratch-dir");
        std::fs::create_dir_all(&plain_tmp).unwrap();
        assert_eq!(project_name_from_path(plain_tmp.to_str().unwrap()), None);

        // 4. Non-existent nested path under container resolves to top-level project
        assert_eq!(
            project_name_from_path("/Users/tester/workdir/web-app/packages/ui/src"),
            Some("web-app".to_string())
        );
    }
}
