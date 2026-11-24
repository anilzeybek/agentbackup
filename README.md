# agentbackup

Fast, single-binary Rust CLI that discovers local conversation logs across AI coding harnesses, extracts clean **user prompts + assistant replies** (stripping images, base64 blobs, tool dumps, thinking traces, and obvious secrets), and backs them up incrementally to a local directory.

## Supported Harnesses

| Harness | Default Local Path | Format |
| :--- | :--- | :--- |
| **Antigravity** | `~/.gemini/antigravity/brain/<uuid>/.system_generated/logs/transcript.jsonl` | JSONL |
| **Claude Code** | `~/.claude/projects/<encoded-path>/<session-id>.jsonl` | JSONL |
| **Codex CLI** | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (and `.jsonl.zst`) | JSONL / Zstd |
| **OpenCode** | `~/.local/share/opencode/opencode.db` (v1.2+) & `storage/` (v1.1) | SQLite / JSON |

All harness adapters are strictly **read-only**.

## Install

```bash
./install.sh
```

Installs the release binary to `~/.local/bin/agentbackup` (override with `INSTALL_DIR=/usr/local/bin ./install.sh`). Or install directly via Cargo:

```bash
cargo install --path .
```

## Quick Start

```bash
# 1. Detect installed harnesses and configure your local backup directory and format
agentbackup init

# 2. Inspect detected sessions and raw disk size vs estimated export size (read-only)
agentbackup scan

# 3. Run an incremental backup
agentbackup run
agentbackup run --dry-run   # preview without modifying files or state

# 4. Check backup health, warnings, and background schedule status
agentbackup status

# 5. Optional: install a macOS launchd background schedule
agentbackup schedule install --every 1h
agentbackup schedule remove
```

## Configuration

Configuration lives at `~/.config/agentbackup/config.toml` (state at `~/.config/agentbackup/state.json`):

```toml
redact_secrets = true
notify = true   # native macOS notifications on errors & schema drift warnings (max once/24h per issue)

[harnesses]
antigravity = "~/.gemini/antigravity/brain"
claude_code = "~/.claude/projects"
codex = "~/.codex/sessions"
opencode = "~/.local/share/opencode"

[[targets]]
name = "local"
type = "local"
path = "/path/to/your/existing/backup-folder"
format = "markdown" # markdown | txt | json
```

Each conversation is written under `<harness>/<project>/<YYYY-MM-DD>_<slug>_<shortid>.<ext>` (with non-project directories normalized to `general/` and nested paths resolved to their Git repo root) alongside a top-level `INDEX` file.

### Guarantees
- **Strict target path check:** If a target `path` does not exist on disk, `agentbackup` exits with an error instead of creating it (preventing silent writes into unmounted sync folders).
- **Never-delete retention:** If a harness prunes old local sessions (such as Claude Code's 30-day default cleanup), previously backed-up conversations remain preserved along with their entries in the top-level `INDEX`.
- **Blob & secret hygiene:** Tool calls, tool outputs, thinking traces, screenshots, and base64 payloads are excluded/replaced with lightweight placeholders, and common API keys/tokens/PEM keys are redacted by default.
- **Drift alerts & health summary:** Healthy background runs stay silent; if a target disappears or a harness changes its transcript schema, `agentbackup` records the warning/error for `agentbackup status` and sends a rate-limited macOS notification.
