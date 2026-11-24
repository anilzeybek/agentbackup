use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::models::Harness;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TargetType {
    #[default]
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    #[default]
    Markdown,
    Txt,
    Json,
}

impl OutputFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Markdown => "md",
            OutputFormat::Txt => "txt",
            OutputFormat::Json => "json",
        }
    }

    pub fn index_filename(&self) -> &'static str {
        match self {
            OutputFormat::Markdown => "INDEX.md",
            OutputFormat::Txt => "INDEX.txt",
            OutputFormat::Json => "index.json",
        }
    }

    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "markdown" | "md" => Some(OutputFormat::Markdown),
            "txt" | "text" => Some(OutputFormat::Txt),
            "json" => Some(OutputFormat::Json),
            _ => None,
        }
    }
}

impl fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputFormat::Markdown => f.write_str("markdown"),
            OutputFormat::Txt => f.write_str("txt"),
            OutputFormat::Json => f.write_str("json"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum OutputLayout {
    #[default]
    PerChat,
    Monthly,
    PerProject,
}

impl OutputLayout {
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "per-chat" | "per_chat" | "chat" => Some(OutputLayout::PerChat),
            "monthly" | "month" => Some(OutputLayout::Monthly),
            "per-project" | "per_project" | "project" => Some(OutputLayout::PerProject),
            _ => None,
        }
    }
}

impl fmt::Display for OutputLayout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputLayout::PerChat => f.write_str("per-chat"),
            OutputLayout::Monthly => f.write_str("monthly"),
            OutputLayout::PerProject => f.write_str("per-project"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessesConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub antigravity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opencode: Option<String>,
}

impl Default for HarnessesConfig {
    fn default() -> Self {
        Self {
            antigravity: Some(default_harness_path_string(Harness::Antigravity)),
            claude_code: Some(default_harness_path_string(Harness::ClaudeCode)),
            codex: Some(default_harness_path_string(Harness::Codex)),
            opencode: Some(default_harness_path_string(Harness::OpenCode)),
        }
    }
}

impl HarnessesConfig {
    pub fn path_for(&self, harness: Harness) -> Option<PathBuf> {
        let raw = match harness {
            Harness::Antigravity => self.antigravity.as_deref(),
            Harness::ClaudeCode => self.claude_code.as_deref(),
            Harness::Codex => self.codex.as_deref(),
            Harness::OpenCode => self.opencode.as_deref(),
        }?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(expand_tilde(trimmed))
    }

    pub fn raw_for(&self, harness: Harness) -> Option<&str> {
        match harness {
            Harness::Antigravity => self.antigravity.as_deref(),
            Harness::ClaudeCode => self.claude_code.as_deref(),
            Harness::Codex => self.codex.as_deref(),
            Harness::OpenCode => self.opencode.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetConfig {
    pub name: String,
    #[serde(rename = "type", default)]
    pub target_type: TargetType,
    pub path: String,
    #[serde(default)]
    pub format: OutputFormat,
    #[serde(default)]
    pub layout: OutputLayout,
}

impl TargetConfig {
    pub fn resolved_path(&self) -> PathBuf {
        expand_tilde(&self.path)
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_true")]
    pub redact_secrets: bool,
    #[serde(default)]
    pub harnesses: HarnessesConfig,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            redact_secrets: true,
            harnesses: HarnessesConfig::default(),
            targets: Vec::new(),
        }
    }
}

impl Config {
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file at {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("Failed to parse TOML config at {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory {}", parent.display())
            })?;
        }
        let serialized = toml::to_string_pretty(self).context("Failed to serialize config")?;
        fs::write(path, serialized)
            .with_context(|| format!("Failed to write config file to {}", path.display()))?;
        Ok(())
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(custom) = env::var("AGENTBACKUP_CONFIG_DIR") {
        if !custom.trim().is_empty() {
            return expand_tilde(&custom);
        }
    }
    home_dir().join(".config").join("agentbackup")
}

pub fn default_config_path() -> PathBuf {
    if let Ok(custom) = env::var("AGENTBACKUP_CONFIG") {
        if !custom.trim().is_empty() {
            return expand_tilde(&custom);
        }
    }
    config_dir().join("config.toml")
}

pub fn default_state_path() -> PathBuf {
    config_dir().join("state.json")
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

pub fn contract_tilde(path: &Path) -> String {
    let home = home_dir();
    if let Ok(stripped) = path.strip_prefix(&home) {
        if stripped.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", stripped.display());
    }
    path.display().to_string()
}

pub fn default_harness_path_string(harness: Harness) -> String {
    match harness {
        Harness::Antigravity => "~/.gemini/antigravity/brain".to_string(),
        Harness::ClaudeCode => {
            if let Ok(dir) = env::var("CLAUDE_CONFIG_DIR") {
                if !dir.trim().is_empty() {
                    return PathBuf::from(dir).join("projects").display().to_string();
                }
            }
            "~/.claude/projects".to_string()
        }
        Harness::Codex => {
            if let Ok(dir) = env::var("CODEX_HOME") {
                if !dir.trim().is_empty() {
                    return PathBuf::from(dir).join("sessions").display().to_string();
                }
            }
            "~/.codex/sessions".to_string()
        }
        Harness::OpenCode => {
            if let Ok(dir) = env::var("XDG_DATA_HOME") {
                if !dir.trim().is_empty() {
                    return PathBuf::from(dir).join("opencode").display().to_string();
                }
            }
            "~/.local/share/opencode".to_string()
        }
    }
}

#[derive(Debug, Clone)]
pub struct DetectedHarness {
    pub harness: Harness,
    pub display_path: String,
    pub resolved_path: PathBuf,
    pub exists: bool,
}

pub fn detect_harnesses(harnesses: &HarnessesConfig) -> Vec<DetectedHarness> {
    Harness::all()
        .iter()
        .map(|&harness| {
            let display_path = harnesses
                .raw_for(harness)
                .map(|s| s.to_string())
                .unwrap_or_else(|| default_harness_path_string(harness));
            let resolved_path = expand_tilde(&display_path);
            let exists = resolved_path.exists();
            DetectedHarness {
                harness,
                display_path,
                resolved_path,
                exists,
            }
        })
        .collect()
}
