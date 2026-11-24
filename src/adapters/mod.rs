pub mod antigravity;
pub mod claude_code;
pub mod codex;
pub mod opencode;

use anyhow::Result;
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

use crate::config::{default_harness_path_string, expand_tilde};
use crate::models::{Conversation, DiscoveredSource, Harness};
use crate::sanitize::sanitize_project_name;

pub trait HarnessAdapter: Send + Sync {
    fn harness(&self) -> Harness;

    fn name(&self) -> &'static str {
        self.harness().as_str()
    }

    fn default_root(&self) -> PathBuf {
        expand_tilde(&default_harness_path_string(self.harness()))
    }

    fn discover(&self, root: &Path) -> Result<Vec<DiscoveredSource>>;

    fn parse(&self, source: &DiscoveredSource) -> Result<Option<Conversation>>;
}

pub fn all_adapters() -> Vec<Box<dyn HarnessAdapter>> {
    vec![
        Box::new(antigravity::AntigravityAdapter),
        Box::new(claude_code::ClaudeCodeAdapter),
        Box::new(codex::CodexAdapter),
        Box::new(opencode::OpenCodeAdapter),
    ]
}

pub fn adapter_for(harness: Harness) -> Box<dyn HarnessAdapter> {
    match harness {
        Harness::Antigravity => Box::new(antigravity::AntigravityAdapter),
        Harness::ClaudeCode => Box::new(claude_code::ClaudeCodeAdapter),
        Harness::Codex => Box::new(codex::CodexAdapter),
        Harness::OpenCode => Box::new(opencode::OpenCodeAdapter),
    }
}

pub fn file_mtime_and_size(path: &Path) -> (u64, u64) {
    match fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (mtime, meta.len())
        }
        Err(_) => (0, 0),
    }
}

pub fn dir_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn re_abs_path() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?:/Users/[^/\s"'`<>|]+(?:/[^/\s"'`<>|]+)+|/home/[^/\s"'`<>|]+(?:/[^/\s"'`<>|]+)+)"#)
            .unwrap()
    })
}

const WORKSPACE_PARENTS: &[&str] = &[
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

const IGNORED_HOME_DIRS: &[&str] = &[
    "Library",
    "Applications",
    "Downloads",
    "Movies",
    "Music",
    "Pictures",
    "Public",
];

/// Infer the most likely `(project_name, project_path)` from absolute file paths encountered in a transcript.
pub fn infer_project_from_text_chunks(texts: &[String]) -> Option<(String, String)> {
    let mut counts: HashMap<(String, String), usize> = HashMap::new();

    for text in texts {
        for m in re_abs_path().find_iter(text) {
            let raw_path = m.as_str().trim_end_matches(|c: char| {
                matches!(c, '.' | ',' | ':' | ';' | ')' | ']' | '}')
            });
            let parts: Vec<&str> = raw_path.split('/').filter(|s| !s.is_empty()).collect();
            // Expected: ["Users" | "home", username, dir1, (dir2), ...]
            if parts.len() < 3 {
                continue;
            }
            let dir1 = parts[2];
            if dir1.starts_with('.') || IGNORED_HOME_DIRS.contains(&dir1) {
                continue;
            }

            let (proj_name, proj_path) =
                if WORKSPACE_PARENTS.contains(&dir1) && parts.len() >= 4 {
                    let dir2 = parts[3];
                    if dir2.starts_with('.') {
                        continue;
                    }
                    (
                        sanitize_project_name(dir2),
                        format!("/{}/{}/{}/{}", parts[0], parts[1], dir1, dir2),
                    )
                } else if !WORKSPACE_PARENTS.contains(&dir1) {
                    (
                        sanitize_project_name(dir1),
                        format!("/{}/{}/{}", parts[0], parts[1], dir1),
                    )
                } else {
                    continue;
                };

            if proj_name != "general" {
                *counts.entry((proj_name, proj_path)).or_insert(0) += 1;
            }
        }
    }

    counts
        .into_iter()
        .max_by(|(k1, c1), (k2, c2)| c1.cmp(c2).then_with(|| k2.0.cmp(&k1.0)))
        .map(|(k, _)| k)
}
