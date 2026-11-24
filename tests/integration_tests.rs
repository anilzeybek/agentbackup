use agentbackup::config::{Config, HarnessesConfig, OutputFormat, TargetConfig, TargetType};
use agentbackup::{init_config_interactive, run_backup, scan_harnesses};
use rusqlite::Connection;
use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;
use tempfile::TempDir;

fn setup_antigravity_fixture(root: &Path) {
    let conv_dir = root.join("340b4a34-b6fc-4246-8695-96038e1633d1");
    let logs_dir = conv_dir.join(".system_generated").join("logs");
    fs::create_dir_all(&logs_dir).unwrap();

    // Add a fake large browser_sandbox blob to verify raw_bytes >> est_export_bytes
    let sandbox_dir = conv_dir.join("browser_sandbox");
    fs::create_dir_all(&sandbox_dir).unwrap();
    fs::write(sandbox_dir.join("recording.webp"), vec![0u8; 64 * 1024]).unwrap();

    let lines = [
        serde_json::json!({
            "step_index": 0,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "created_at": "2026-11-24T16:34:52Z",
            "content": "<USER_REQUEST>\nFix auth token refresh in my-web-app with key sk-ant-api03-1234567890abcdefghijklmno\nHere is a screenshot: ![shot](data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==)\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-11-24T16:34:52Z.\n</ADDITIONAL_METADATA>"
        }),
        serde_json::json!({
            "step_index": 1,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "model": "gemini-4-neon-high",
            "created_at": "2026-11-24T16:34:55Z",
            "thinking": "Internal thinking that must never be exported",
            "tool_calls": [{
                "name": "read_file",
                "args": {"Path": "/Users/tester/workdir/my-web-app/src/auth.ts"}
            }]
        }),
        serde_json::json!({
            "step_index": 2,
            "source": "MODEL",
            "type": "GENERIC",
            "created_at": "2026-11-24T16:34:56Z",
            "content": "MASSIVE TOOL OUTPUT DUMP THAT SHOULD BE EXCLUDED"
        }),
        serde_json::json!({
            "step_index": 3,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "model": "gemini-4-neon-high",
            "created_at": "2026-11-24T16:35:05Z",
            "thinking": "More internal thinking",
            "content": "Updated the refresh flow in auth.ts to handle 401 retries cleanly."
        }),
        serde_json::json!({
            "step_index": 4,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "created_at": "2026-11-24T16:36:00Z",
            "content": "<USER_REQUEST>\nDouble-check edge cases with another model\n</USER_REQUEST>"
        }),
        serde_json::json!({
            "step_index": 5,
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "model": "gemini-4-pro",
            "created_at": "2026-11-24T16:36:14Z",
            "content": "Verified concurrent refresh locking as well."
        }),
    ];

    let mut content = String::new();
    for line in lines {
        content.push_str(&line.to_string());
        content.push('\n');
    }
    fs::write(logs_dir.join("transcript.jsonl"), content).unwrap();
}

fn setup_claude_code_fixture(root: &Path) {
    let proj_dir = root.join("-Users-tester-workdir-my-web-app");
    fs::create_dir_all(&proj_dir).unwrap();

    let lines = [
        serde_json::json!({
            "type": "summary",
            "summary": "Add Stripe webhook handler"
        }),
        serde_json::json!({
            "type": "user",
            "cwd": "/Users/tester/workdir/my-web-app",
            "gitBranch": "feat/stripe-webhooks",
            "version": "2.0.31",
            "timestamp": "2026-11-22T10:00:05Z",
            "message": {
                "role": "user",
                "content": [
                    {"type": "text", "text": "Please add a Stripe webhook endpoint"},
                    {"type": "image", "source": {"type": "base64", "data": "AAAA"}}
                ]
            }
        }),
        serde_json::json!({
            "type": "user",
            "cwd": "/Users/tester/workdir/my-web-app",
            "gitBranch": "feat/stripe-webhooks",
            "version": "2.0.31",
            "timestamp": "2026-11-22T10:00:07Z",
            "message": {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "SECRET_TOOL_OUTPUT"}
                ]
            }
        }),
        serde_json::json!({
            "type": "assistant",
            "isSidechain": true,
            "timestamp": "2026-11-22T10:00:08Z",
            "message": {"id": "side_1", "role": "assistant", "model": "claude-haiku-4-5", "content": [{"type": "text", "text": "SIDECHAIN_NOISE"}]}
        }),
        serde_json::json!({
            "type": "assistant",
            "cwd": "/Users/tester/workdir/my-web-app",
            "gitBranch": "feat/stripe-webhooks",
            "version": "2.0.31",
            "timestamp": "2026-11-22T10:00:19Z",
            "message": {
                "id": "msg_01",
                "role": "assistant",
                "model": "claude-sonnet-4-5-20250929",
                "content": [
                    {"type": "thinking", "thinking": "HIDDEN_CLAUDE_THINKING"},
                    {"type": "tool_use", "id": "t1", "name": "Edit", "input": {}},
                    {"type": "text", "text": "I added the webhook route and signature verification."}
                ]
            }
        }),
    ];

    let mut content = String::new();
    for line in lines {
        content.push_str(&line.to_string());
        content.push('\n');
    }
    fs::write(proj_dir.join("91fc02ab-1234-5678-9abc-def012345678.jsonl"), content).unwrap();

    fs::write(
        proj_dir.join("agent-sub123.jsonl"),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"skip me\"}}\n",
    )
    .unwrap();
}

fn setup_codex_fixtures(root: &Path) {
    let day_dir = root.join("2026").join("11").join("23");
    fs::create_dir_all(&day_dir).unwrap();

    // 1. Plain .jsonl rollout with session_meta + turn_context model/branch/cli_version
    let plain_uuid = "019b1111-2222-7333-8444-555566667777";
    let plain_lines = [
        serde_json::json!({
            "timestamp": "2026-11-23T09:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": plain_uuid,
                "cwd": "/Users/tester/workdir/cli-tools",
                "cli_version": "0.42.0",
                "git": { "branch": "main" },
                "timestamp": "2026-11-23T09:00:00Z"
            }
        }),
        serde_json::json!({
            "timestamp": "2026-11-23T09:00:01Z",
            "type": "turn_context",
            "payload": {
                "cwd": "/Users/tester/workdir/cli-tools",
                "model": "gpt-5-codex"
            }
        }),
        serde_json::json!({
            "timestamp": "2026-11-23T09:00:02Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "<environment_context>\nOS: macOS\n</environment_context>"}]
            }
        }),
        serde_json::json!({
            "timestamp": "2026-11-23T09:00:05Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Write a Makefile for the Rust CLI"},
                    {"type": "input_image", "image_url": "data:image/png;base64,BBB"}
                ]
            }
        }),
        serde_json::json!({
            "timestamp": "2026-11-23T09:00:06Z",
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "shell",
                "arguments": "{\"cmd\":\"ls\"}"
            }
        }),
        serde_json::json!({
            "timestamp": "2026-11-23T09:00:17Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Created a Makefile with build, test, and install targets."}]
            }
        }),
        serde_json::json!({
            "timestamp": "2026-11-23T09:00:17Z",
            "type": "event_msg",
            "payload": {
                "type": "agent_message",
                "message": "Created a Makefile with build, test, and install targets."
            }
        }),
    ];
    let mut plain_body = String::new();
    for line in plain_lines {
        plain_body.push_str(&line.to_string());
        plain_body.push('\n');
    }
    fs::write(
        day_dir.join(format!("rollout-2026-11-23T09-00-00-{}.jsonl", plain_uuid)),
        plain_body,
    )
    .unwrap();

    // 2. Zstd-compressed .jsonl.zst rollout
    let zst_uuid = "019b8888-9999-7aaa-8bbb-ccccddddeeee";
    let zst_lines = [
        serde_json::json!({
            "timestamp": "2026-11-23T12:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": zst_uuid,
                "cwd": "/Users/tester/workdir/cli-tools",
                "cli_version": "0.41.0",
                "model": "o4-mini",
                "timestamp": "2026-11-23T12:00:00Z"
            }
        }),
        serde_json::json!({
            "timestamp": "2026-11-23T12:00:05Z",
            "type": "event_msg",
            "payload": {
                "type": "user_message",
                "message": "Compress older logs with zstd"
            }
        }),
        serde_json::json!({
            "timestamp": "2026-11-23T12:00:15Z",
            "type": "event_msg",
            "payload": {
                "type": "agent_message",
                "message": "Done, zstd streaming decompression is wired up."
            }
        }),
    ];
    let mut zst_raw = String::new();
    for line in zst_lines {
        zst_raw.push_str(&line.to_string());
        zst_raw.push('\n');
    }
    let zst_file = fs::File::create(
        day_dir.join(format!("rollout-2026-11-23T12-00-00-{}.jsonl.zst", zst_uuid)),
    )
    .unwrap();
    let mut encoder = zstd::stream::write::Encoder::new(zst_file, 3).unwrap();
    encoder.write_all(zst_raw.as_bytes()).unwrap();
    encoder.finish().unwrap();
}

fn setup_opencode_fixtures(root: &Path) {
    fs::create_dir_all(root).unwrap();

    // 1. SQLite v1.2+ DB with version and modelID
    let db_path = root.join("opencode.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "
        CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT);
        CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, title TEXT, directory TEXT, version TEXT, time_created INTEGER, time_updated INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
        ",
    )
    .unwrap();

    conn.execute(
        "INSERT INTO project (id, worktree) VALUES (?1, ?2)",
        ["proj_1", "/Users/tester/workdir/opencode-demo"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session (id, project_id, title, directory, version, time_created, time_updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            "ses_sqlite_01",
            "proj_1",
            "Refactor SQLite queries",
            "/Users/tester/workdir/opencode-demo",
            "1.2.8",
            1795440005000i64,
            1795440612000i64
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            "msg_u1",
            "ses_sqlite_01",
            1795440005000i64,
            r#"{"role":"user"}"#
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "prt_1",
            "msg_u1",
            "ses_sqlite_01",
            1795440005000i64,
            r#"{"type":"text","text":"Optimize the SQLite reader"}"#
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "prt_2",
            "msg_u1",
            "ses_sqlite_01",
            1795440006000i64,
            r#"{"type":"file","mime":"image/png"}"#
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            "msg_a1",
            "ses_sqlite_01",
            1795440012000i64,
            r#"{"role":"assistant","providerID":"anthropic","modelID":"claude-opus-4-5"}"#
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "prt_3",
            "msg_a1",
            "ses_sqlite_01",
            1795440012000i64,
            r#"{"type":"reasoning","text":"HIDDEN_REASONING"}"#
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "prt_4",
            "msg_a1",
            "ses_sqlite_01",
            1795440013000i64,
            r#"{"type":"tool","tool":"bash","state":{"output":"HIDDEN_TOOL"}}"#
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "prt_5",
            "msg_a1",
            "ses_sqlite_01",
            1795440014000i64,
            r#"{"type":"text","text":"Prepared statements now reuse a read-only connection."}"#
        ],
    )
    .unwrap();

    // 2. Legacy v1.1 JSON storage
    let sess_dir = root.join("storage").join("session").join("proj_legacy");
    let msg_dir = root.join("storage").join("message").join("ses_legacy_02");
    let part_u = root.join("storage").join("part").join("msg_lu1");
    let part_a = root.join("storage").join("part").join("msg_la1");
    fs::create_dir_all(&sess_dir).unwrap();
    fs::create_dir_all(&msg_dir).unwrap();
    fs::create_dir_all(&part_u).unwrap();
    fs::create_dir_all(&part_a).unwrap();

    fs::write(
        sess_dir.join("ses_legacy_02.json"),
        serde_json::json!({
            "id": "ses_legacy_02",
            "projectID": "proj_legacy",
            "directory": "/Users/tester/workdir/legacy-app",
            "version": "1.1.4",
            "title": "Legacy OpenCode session",
            "time": {"created": 1795353600000i64, "updated": 1795353700000i64}
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        msg_dir.join("msg_lu1.json"),
        serde_json::json!({"id": "msg_lu1", "role": "user", "time": {"created": 1795353600000i64}})
            .to_string(),
    )
    .unwrap();
    fs::write(
        part_u.join("part_1.json"),
        serde_json::json!({"id": "part_1", "type": "text", "text": "Explain legacy JSON format"})
            .to_string(),
    )
    .unwrap();
    fs::write(
        msg_dir.join("msg_la1.json"),
        serde_json::json!({"id": "msg_la1", "role": "assistant", "modelID": "gpt-4.1", "time": {"created": 1795353660000i64}})
            .to_string(),
    )
    .unwrap();
    fs::write(
        part_a.join("part_2.json"),
        serde_json::json!({"id": "part_2", "type": "text", "text": "Legacy OpenCode splits session, message, and part JSON files."})
            .to_string(),
    )
    .unwrap();
}

fn build_fixture_harnesses(base: &Path) -> HarnessesConfig {
    let ag = base.join("antigravity");
    let cc = base.join("claude_code");
    let cx = base.join("codex");
    let oc = base.join("opencode");
    setup_antigravity_fixture(&ag);
    setup_claude_code_fixture(&cc);
    setup_codex_fixtures(&cx);
    setup_opencode_fixtures(&oc);

    HarnessesConfig {
        antigravity: Some(ag.display().to_string()),
        claude_code: Some(cc.display().to_string()),
        codex: Some(cx.display().to_string()),
        opencode: Some(oc.display().to_string()),
    }
}

#[test]
fn scan_and_per_chat_markdown_backup_with_metadata_incremental_force_and_retention() {
    let tmp = TempDir::new().unwrap();
    let harnesses = build_fixture_harnesses(tmp.path());
    let backup_dir = tmp.path().join("backups");
    fs::create_dir_all(&backup_dir).unwrap();
    let state_path = tmp.path().join("state.json");

    let cfg = Config {
        redact_secrets: true,
        harnesses: harnesses.clone(),
        targets: vec![TargetConfig {
            name: "local".to_string(),
            target_type: TargetType::Local,
            path: backup_dir.display().to_string(),
            format: OutputFormat::Markdown,
        }],
    };

    // 1. Scan reports all 6 sessions across all 4 harnesses and raw size > est export size
    let summaries = scan_harnesses(&cfg).unwrap();
    let total_sessions: usize = summaries.iter().map(|s| s.session_count).sum();
    assert_eq!(total_sessions, 6);
    let ag_summary = summaries
        .iter()
        .find(|s| s.harness.as_str() == "antigravity")
        .unwrap();
    assert!(ag_summary.raw_bytes > 60_000);
    assert!(ag_summary.est_export_bytes < 2_500);

    // 2. Dry-run previews 6 new conversations without creating files or state
    let dry_reports = run_backup(&cfg, &state_path, true, false).unwrap();
    assert_eq!(dry_reports[0].stats.new_count, 6);
    assert!(!state_path.exists());
    assert!(!backup_dir.join("INDEX.md").exists());

    // 3. First real run writes 6 conversation files + INDEX.md
    let reports = run_backup(&cfg, &state_path, false, false).unwrap();
    assert_eq!(reports[0].stats.new_count, 6);
    assert_eq!(reports[0].stats.updated_count, 0);
    assert_eq!(reports[0].stats.skipped_count, 0);
    assert!(backup_dir.join("INDEX.md").is_file());

    // Verify Antigravity + Claude Code markdown metadata, sanitization, exact timestamps, and multi-model headings
    let web_app_dir = backup_dir.join("my-web-app");
    let mut ag_md = String::new();
    let mut cc_md = String::new();
    for entry in fs::read_dir(&web_app_dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains("_antigravity_") {
            ag_md = fs::read_to_string(entry.path()).unwrap();
        } else if name.contains("_claude-code_") {
            cc_md = fs::read_to_string(entry.path()).unwrap();
        }
    }

    // Antigravity: multi-model, exact timestamps with seconds, workspace inferred, omitted None fields
    assert!(ag_md.contains("workspace: \"/Users/tester/workdir/my-web-app\""));
    assert!(ag_md.contains("models:\n  - gemini-4-neon-high\n  - gemini-4-pro\n"));
    assert!(ag_md.contains("created_at: 2026-11-24T16:34:52Z"));
    assert!(ag_md.contains("updated_at: 2026-11-24T16:36:14Z"));
    assert!(ag_md.contains("message_count: 4"));
    assert!(!ag_md.contains("git_branch:"));
    assert!(!ag_md.contains("harness_version:"));
    assert!(ag_md.contains("## User · 2026-11-24 16:34:52 UTC"));
    assert!(ag_md.contains("## Assistant (gemini-4-neon-high) · 2026-11-24 16:35:05 UTC"));
    assert!(ag_md.contains("## Assistant (gemini-4-pro) · 2026-11-24 16:36:14 UTC"));
    assert!(ag_md.contains("[REDACTED:ANTHROPIC_KEY]"));
    assert!(ag_md.contains("[image omitted]"));
    assert!(!ag_md.contains("sk-ant-api03"));
    assert!(!ag_md.contains("<USER_REQUEST>"));
    assert!(!ag_md.contains("Internal thinking"));
    assert!(!ag_md.contains("MASSIVE TOOL OUTPUT DUMP"));

    // Claude Code: single model, git_branch, harness_version, workspace, exact timestamps
    assert!(cc_md.contains("harness_version: \"2.0.31\""));
    assert!(cc_md.contains("workspace: \"/Users/tester/workdir/my-web-app\""));
    assert!(cc_md.contains("git_branch: \"feat/stripe-webhooks\""));
    assert!(cc_md.contains("models:\n  - claude-sonnet-4-5-20250929\n"));
    assert!(cc_md.contains("message_count: 2"));
    assert!(cc_md.contains("## User · 2026-11-22 10:00:05 UTC"));
    assert!(cc_md.contains("## Assistant · 2026-11-22 10:00:19 UTC"));
    assert!(!cc_md.contains("## Assistant (claude-sonnet-4-5-20250929)"));
    assert!(cc_md.contains("[image omitted]"));
    assert!(!cc_md.contains("SECRET_TOOL_OUTPUT"));
    assert!(!cc_md.contains("HIDDEN_CLAUDE_THINKING"));
    assert!(!cc_md.contains("SIDECHAIN_NOISE"));

    // Verify OpenCode SQLite markdown metadata
    let oc_dir = backup_dir.join("opencode-demo");
    let oc_entry = fs::read_dir(&oc_dir).unwrap().flatten().next().unwrap();
    let oc_md = fs::read_to_string(oc_entry.path()).unwrap();
    assert!(oc_md.contains("harness_version: \"1.2.8\""));
    assert!(oc_md.contains("workspace: \"/Users/tester/workdir/opencode-demo\""));
    assert!(oc_md.contains("models:\n  - claude-opus-4-5\n"));
    assert!(oc_md.contains("message_count: 2"));

    // Verify INDEX.md includes date, harness, project, models, message count, and title
    let index_md = fs::read_to_string(backup_dir.join("INDEX.md")).unwrap();
    assert!(index_md.contains("| Date | Harness | Project | Models | Messages | Title | File |"));
    assert!(index_md.contains("gemini-4-neon-high, gemini-4-pro"));
    assert!(index_md.contains("claude-sonnet-4-5-20250929"));
    assert!(index_md.contains("gpt-5-codex"));
    assert!(index_md.contains("claude-opus-4-5"));

    // 4. Second run with no changes skips all 6 conversations
    let second = run_backup(&cfg, &state_path, false, false).unwrap();
    assert_eq!(second[0].stats.new_count, 0);
    assert_eq!(second[0].stats.updated_count, 0);
    assert_eq!(second[0].stats.skipped_count, 6);

    // 5. --force re-exports all 6 conversations even though unchanged
    let forced = run_backup(&cfg, &state_path, false, true).unwrap();
    assert_eq!(forced[0].stats.updated_count, 6);
    assert_eq!(forced[0].stats.skipped_count, 0);

    // 6. Never-delete guarantee: delete Claude Code local source, re-run, verify backup file & INDEX retain it
    let cc_root = Path::new(harnesses.claude_code.as_ref().unwrap());
    fs::remove_dir_all(cc_root).unwrap();
    let after_delete = run_backup(&cfg, &state_path, false, false).unwrap();
    assert_eq!(after_delete[0].stats.total_tracked, 6);
    let still_has_cc = fs::read_dir(&web_app_dir)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains("_claude-code_"));
    assert!(still_has_cc, "Backed-up Claude Code file must be retained");
    let index_after = fs::read_to_string(backup_dir.join("INDEX.md")).unwrap();
    assert!(index_after.contains("Add Stripe webhook handler"));
}

#[test]
fn missing_target_directory_returns_clear_error_without_creating_it() {
    let tmp = TempDir::new().unwrap();
    let harnesses = build_fixture_harnesses(tmp.path());
    let missing_dir = tmp.path().join("does-not-exist-mount");
    let state_path = tmp.path().join("state.json");

    let cfg = Config {
        redact_secrets: true,
        harnesses,
        targets: vec![TargetConfig {
            name: "local".to_string(),
            target_type: TargetType::Local,
            path: missing_dir.display().to_string(),
            format: OutputFormat::Markdown,
        }],
    };

    let err = run_backup(&cfg, &state_path, false, false).unwrap_err();
    assert!(err.to_string().contains("does not exist"));
    assert!(
        !missing_dir.exists(),
        "Target directory must never be auto-created"
    );
}

#[test]
fn txt_and_json_formats_include_full_metadata_and_preserve_deleted_sessions() {
    let tmp = TempDir::new().unwrap();
    let harnesses = build_fixture_harnesses(tmp.path());
    let state_path = tmp.path().join("state.json");

    let txt_dir = tmp.path().join("txt_out");
    let json_dir = tmp.path().join("json_out");
    fs::create_dir_all(&txt_dir).unwrap();
    fs::create_dir_all(&json_dir).unwrap();

    let cfg = Config {
        redact_secrets: true,
        harnesses: harnesses.clone(),
        targets: vec![
            TargetConfig {
                name: "txt-target".to_string(),
                target_type: TargetType::Local,
                path: txt_dir.display().to_string(),
                format: OutputFormat::Txt,
            },
            TargetConfig {
                name: "json-target".to_string(),
                target_type: TargetType::Local,
                path: json_dir.display().to_string(),
                format: OutputFormat::Json,
            },
        ],
    };

    let reports = run_backup(&cfg, &state_path, false, false).unwrap();
    assert_eq!(reports.len(), 2);
    assert!(txt_dir.join("INDEX.txt").is_file());
    assert!(json_dir.join("index.json").is_file());

    // Verify JSON per-chat exports for Codex (both plain .jsonl and .jsonl.zst)
    let cli_json_dir = json_dir.join("cli-tools");
    let mut codex_convs = Vec::new();
    for entry in fs::read_dir(&cli_json_dir).unwrap().flatten() {
        let raw = fs::read_to_string(entry.path()).unwrap();
        let val: serde_json::Value = serde_json::from_str(&raw).unwrap();
        codex_convs.push(val);
    }
    assert_eq!(codex_convs.len(), 2);

    let plain_codex = codex_convs
        .iter()
        .find(|c| c["id"] == "codex:019b1111-2222-7333-8444-555566667777")
        .unwrap();
    assert_eq!(plain_codex["workspace"], "/Users/tester/workdir/cli-tools");
    assert_eq!(plain_codex["git_branch"], "main");
    assert_eq!(plain_codex["harness_version"], "0.42.0");
    assert_eq!(plain_codex["models"], serde_json::json!(["gpt-5-codex"]));
    assert_eq!(plain_codex["message_count"], 2);
    assert_eq!(plain_codex["messages"][1]["model"], "gpt-5-codex");
    assert_eq!(
        plain_codex["messages"][1]["timestamp"],
        "2026-11-23T09:00:17Z"
    );

    let zst_codex = codex_convs
        .iter()
        .find(|c| c["id"] == "codex:019b8888-9999-7aaa-8bbb-ccccddddeeee")
        .unwrap();
    assert_eq!(zst_codex["harness_version"], "0.41.0");
    assert_eq!(zst_codex["models"], serde_json::json!(["o4-mini"]));
    assert!(zst_codex.get("git_branch").is_none());

    // Verify index.json includes models and message_count
    let idx_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(json_dir.join("index.json")).unwrap()).unwrap();
    assert_eq!(idx_json["total"], 6);
    assert!(idx_json["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["models"] == serde_json::json!(["gpt-5-codex"]) && c["message_count"] == 2));

    // Verify TXT per-chat headers and INDEX.txt
    let txt_web_dir = txt_dir.join("my-web-app");
    let mut ag_txt = String::new();
    let mut cc_txt = String::new();
    for entry in fs::read_dir(&txt_web_dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains("_antigravity_") {
            ag_txt = fs::read_to_string(entry.path()).unwrap();
        } else if name.contains("_claude-code_") {
            cc_txt = fs::read_to_string(entry.path()).unwrap();
        }
    }
    assert!(cc_txt.contains("Git Branch: feat/stripe-webhooks\n"));
    assert!(cc_txt.contains("Harness Version: 2.0.31\n"));
    assert!(ag_txt.contains("Models: gemini-4-neon-high, gemini-4-pro\n"));
    assert!(ag_txt.contains("[Assistant (gemini-4-pro) · 2026-11-24 16:36:14 UTC]"));

    let idx_txt = fs::read_to_string(txt_dir.join("INDEX.txt")).unwrap();
    assert!(idx_txt.contains("gemini-4-neon-high, gemini-4-pro"));
    assert!(idx_txt.contains("gpt-5-codex"));

    // Delete Claude Code locally AND append a new turn to Antigravity; verify Claude Code export stays retained
    fs::remove_dir_all(Path::new(harnesses.claude_code.as_ref().unwrap())).unwrap();
    let ag_transcript = Path::new(harnesses.antigravity.as_ref().unwrap())
        .join("340b4a34-b6fc-4246-8695-96038e1633d1")
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&ag_transcript)
        .unwrap();
    writeln!(
        f,
        "{}",
        serde_json::json!({
            "step_index": 6,
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "created_at": "2026-11-24T17:00:00Z",
            "content": "<USER_REQUEST>\nFollow-up check on token expiry\n</USER_REQUEST>"
        })
    )
    .unwrap();

    let after = run_backup(&cfg, &state_path, false, false).unwrap();
    assert_eq!(after[0].stats.updated_count, 1);
    assert_eq!(after[0].stats.total_tracked, 6);

    let still_has_cc_txt = fs::read_dir(&txt_web_dir)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains("_claude-code_"));
    assert!(still_has_cc_txt);
}

#[test]
fn init_wizard_rejects_missing_path_then_saves_valid_config() {
    let tmp = TempDir::new().unwrap();
    let valid_dir = tmp.path().join("my-backups");
    fs::create_dir_all(&valid_dir).unwrap();
    let missing_dir = tmp.path().join("nope");
    let cfg_path = tmp.path().join("config.toml");

    let input = format!("{}\n{}\ntxt\n", missing_dir.display(), valid_dir.display());
    let mut reader = Cursor::new(input.as_bytes());
    let mut output = Vec::new();

    let cfg = init_config_interactive(&cfg_path, &mut reader, &mut output).unwrap();
    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("does not exist"));
    assert!(cfg_path.is_file());
    assert_eq!(cfg.targets.len(), 1);
    assert_eq!(cfg.targets[0].format, OutputFormat::Txt);
}
