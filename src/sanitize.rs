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

/// Extract a project directory name from a filesystem path string if it looks like a workspace path.
pub fn project_name_from_path(path_str: &str) -> Option<String> {
    let p = std::path::Path::new(path_str.trim());
    let name = p.file_name()?.to_str()?;
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
}
