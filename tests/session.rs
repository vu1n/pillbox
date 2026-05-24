//! End-to-end tests for `pillbox session {list,info,attach,detach,rm}`
//! and the `--detach` flag on `pillbox run --remote`.
//!
//! Live E2B isn't reachable from CI, so we cover the registry surface
//! (list / info / detach / rm) by planting a session TOML on disk and
//! invoking the real binary against it. The attach / kill helper
//! subprocess paths are exercised by the unit-test suite in
//! `src/sandbox/remote_e2b.rs`.

mod common;

use std::fs;

use tempfile::TempDir;

use common::{assert_ok, run};

/// Drop a synthetic session record into a freshly-`init`-ed pillbox so
/// the registry-side commands have something to chew on. The record
/// shape mirrors what `RemoteE2bSandbox::run` would write, minus an
/// `attached_pid` (the session starts detached).
fn plant_session(home: &std::path::Path, id: &str, remote: &str, label: Option<&str>) {
    plant_session_with_attached(home, id, remote, label, None);
}

/// Like [`plant_session`] but also stamps an `attached_pid` so detach-
/// path tests can hit the SIGTERM branch.
fn plant_session_with_attached(
    home: &std::path::Path,
    id: &str,
    remote: &str,
    label: Option<&str>,
    attached_pid: Option<i64>,
) {
    let dir = home.join(".pillbox/global/sessions");
    fs::create_dir_all(&dir).unwrap();
    let label_line = label
        .map(|l| format!("\nlabel = \"{l}\""))
        .unwrap_or_default();
    let attached_line = attached_pid
        .map(|p| format!("\nattached_pid = {p}"))
        .unwrap_or_default();
    let body = format!(
        "id = \"{id}\"\n\
         remote = \"{remote}\"\n\
         backend = \"e2b\"\n\
         sandbox_id = \"sb_test_{id}\"\n\
         pty_pid = 42\n\
         agent_id = \"claude\"\n\
         started_at = \"2026-05-21T00:00:00Z\"{label_line}{attached_line}\n",
    );
    fs::write(dir.join(format!("{id}.toml")), body).unwrap();
}

/// Plant a session with an explicit `expires_at` timestamp. Used by
/// `session prune` tests so we can deterministically place records in
/// the past or future without sleeping.
fn plant_session_with_expiry(home: &std::path::Path, id: &str, remote: &str, expires_at: &str) {
    let dir = home.join(".pillbox/global/sessions");
    fs::create_dir_all(&dir).unwrap();
    let body = format!(
        "id = \"{id}\"\n\
         remote = \"{remote}\"\n\
         backend = \"e2b\"\n\
         sandbox_id = \"sb_test_{id}\"\n\
         pty_pid = 42\n\
         agent_id = \"claude\"\n\
         started_at = \"2026-05-21T00:00:00Z\"\n\
         expires_at = \"{expires_at}\"\n",
    );
    fs::write(dir.join(format!("{id}.toml")), body).unwrap();
}

#[test]
fn list_empty_emits_friendly_message() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");

    let out = run(home.path(), cwd.path(), &["session", "list"]);
    assert_ok(&out, "session list");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no sessions"), "got: {stdout}");
    assert!(stdout.contains("--detach"), "got: {stdout}");
}

#[test]
fn list_json_is_stable_v1() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session(home.path(), "abc123def456", "cloud", Some("nightly"));

    let out = run(home.path(), cwd.path(), &["session", "list", "--json"]);
    assert_ok(&out, "session list --json");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(parsed["version"], serde_json::json!(1));
    let sessions = parsed["sessions"].as_array().expect("array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], serde_json::json!("abc123def456"));
    assert_eq!(sessions[0]["backend"], serde_json::json!("e2b"));
    assert_eq!(sessions[0]["label"], serde_json::json!("nightly"));
    assert_eq!(sessions[0]["attached_pid"], serde_json::Value::Null);
}

#[test]
fn info_resolves_by_prefix() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session(home.path(), "abcdef123456", "cloud", None);

    let out = run(
        home.path(),
        cwd.path(),
        &["session", "info", "abcdef", "--json"],
    );
    assert_ok(&out, "session info");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(parsed["session"]["id"], serde_json::json!("abcdef123456"));
    assert_eq!(
        parsed["session"]["sandbox_id"],
        serde_json::json!("sb_test_abcdef123456")
    );
}

#[test]
fn info_rejects_short_prefix() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session(home.path(), "abcdef123456", "cloud", None);

    let out = run(home.path(), cwd.path(), &["session", "info", "abc"]);
    assert!(!out.status.success(), "should reject 3-char prefix");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("too short"), "got: {stderr}");
}

#[test]
fn info_rejects_ambiguous_prefix() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session(home.path(), "abcdef000001", "cloud", None);
    plant_session(home.path(), "abcdef000002", "cloud", None);

    let out = run(home.path(), cwd.path(), &["session", "info", "abcdef"]);
    assert!(!out.status.success(), "should reject ambiguous prefix");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("matches 2"), "got: {stderr}");
}

#[test]
fn session_done_emits_completed_event() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session(home.path(), "abcdef111111", "cloud", None);

    let out = run(
        home.path(),
        cwd.path(),
        &[
            "session",
            "done",
            "abcdef111111",
            "--status",
            "ok",
            "--exit-code",
            "0",
        ],
    );
    assert_ok(&out, "session done");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("marked completed"), "got: {stdout}");

    // The event should have landed in events.jsonl.
    let events_path = home.path().join(".pillbox/global/events.jsonl");
    let body = fs::read_to_string(&events_path).expect("events.jsonl exists");
    let last_line = body.lines().last().expect("at least one event");
    let event: serde_json::Value = serde_json::from_str(last_line).expect("json");
    assert_eq!(event["event"], "session.completed");
    assert_eq!(event["session_id"], "abcdef111111");
    assert_eq!(event["status"], "ok");
    assert_eq!(event["exit_code"], 0);
}

#[test]
fn session_done_failed_carries_reason_and_exit_code() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session(home.path(), "abcdef222222", "cloud", None);

    let out = run(
        home.path(),
        cwd.path(),
        &[
            "session",
            "done",
            "abcdef222222",
            "--status",
            "failed",
            "--reason",
            "agent panic at line 42",
            "--exit-code",
            "139",
        ],
    );
    assert_ok(&out, "session done failed");

    let events_path = home.path().join(".pillbox/global/events.jsonl");
    let body = fs::read_to_string(&events_path).expect("events.jsonl exists");
    let last_line = body.lines().last().expect("at least one event");
    let event: serde_json::Value = serde_json::from_str(last_line).expect("json");
    assert_eq!(event["event"], "session.failed");
    assert_eq!(event["status"], "error");
    assert_eq!(event["reason"], "agent panic at line 42");
    assert_eq!(event["exit_code"], 139);
}

#[test]
fn session_done_persists_result_snapshot_on_record() {
    // When `--result-snapshot HANDLE` is passed AND the session
    // record exists, the record is mutated so future `session pull`
    // / `session info` can find the snapshot. Stub-mode (no record)
    // skips the persist — covered by the next test.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session(home.path(), "abcdef333333", "cloud", None);

    let out = run(
        home.path(),
        cwd.path(),
        &[
            "session",
            "done",
            "abcdef333333",
            "--status",
            "ok",
            "--result-snapshot",
            "snap-deadbeef",
        ],
    );
    assert_ok(&out, "session done with result-snapshot");

    let out = run(
        home.path(),
        cwd.path(),
        &["session", "info", "abcdef333333", "--json"],
    );
    assert_ok(&out, "session info");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(parsed["session"]["result_snapshot"], "snap-deadbeef");

    // Also verify the event payload carries it.
    let events_path = home.path().join(".pillbox/global/events.jsonl");
    let body = fs::read_to_string(&events_path).expect("events.jsonl exists");
    let last_line = body.lines().last().expect("at least one event");
    let event: serde_json::Value = serde_json::from_str(last_line).expect("json");
    assert_eq!(event["result_snapshot"], "snap-deadbeef");
}

#[test]
fn session_prune_dry_run_lists_expired_only() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    // One past, one future, one with no expiry — only the past one
    // is expired.
    plant_session_with_expiry(
        home.path(),
        "expired00aaaa",
        "cloud",
        "2000-01-01T00:00:00Z",
    );
    plant_session_with_expiry(
        home.path(),
        "future0000bbbb",
        "cloud",
        "2099-01-01T00:00:00Z",
    );
    plant_session(home.path(), "noexpiry000cc", "cloud", None);

    let out = run(home.path(), cwd.path(), &["session", "prune", "--dry-run"]);
    assert_ok(&out, "session prune --dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("would prune 1 session"), "got: {stdout}");
    assert!(stdout.contains("expired00aaaa"), "got: {stdout}");
    assert!(
        !stdout.contains("future0000bbbb"),
        "future shouldn't be in dry-run output: {stdout}"
    );
    assert!(
        !stdout.contains("noexpiry000cc"),
        "no-expiry shouldn't be in dry-run output: {stdout}"
    );
}

#[test]
fn session_prune_empty_is_friendly() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    // Only future + no-expiry sessions — nothing to prune.
    plant_session_with_expiry(
        home.path(),
        "future0000bbbb",
        "cloud",
        "2099-01-01T00:00:00Z",
    );
    plant_session(home.path(), "noexpiry000cc", "cloud", None);

    let out = run(home.path(), cwd.path(), &["session", "prune"]);
    assert_ok(&out, "session prune");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("nothing to prune"), "got: {stdout}");
}

#[test]
fn session_prune_warns_on_malformed_expires_at() {
    // A corrupt `expires_at` should NOT be silently dropped (could
    // leak records forever), but should be surfaced so the user can
    // fix the record manually. Prune walks the malformed value into
    // stderr and continues with the rest.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session_with_expiry(home.path(), "garbage000eee", "cloud", "not a timestamp");

    let out = run(home.path(), cwd.path(), &["session", "prune", "--dry-run"]);
    assert_ok(&out, "session prune --dry-run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("malformed expires_at") && stderr.contains("garbage000eee"),
        "expected malformed warning in stderr, got: {stderr}"
    );
    // And the record stays put — prune didn't drop it.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("nothing to prune"),
        "malformed record should not be pruned, got: {stdout}"
    );
}

#[test]
fn run_ttl_rejects_malformed_duration() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["new", "--name", "proj"]);
    assert_ok(&out, "new");

    let out = run(
        home.path(),
        cwd.path(),
        &[
            "run", "--remote", "nope", "--detach", "--ttl", "5y", "--", "ignored",
        ],
    );
    assert!(!out.status.success(), "5y unit should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unsupported unit"), "got: {stderr}");
}

#[test]
fn session_pull_errors_without_result_snapshot() {
    // Default state: session record exists but the agent hasn't
    // finished (or `session done --result-snapshot` was never
    // called). `session pull` should error with an actionable hint
    // rather than silently doing nothing.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session(home.path(), "abcdef444444", "cloud", None);

    let out = run(
        home.path(),
        cwd.path(),
        &["session", "pull", "abcdef444444"],
    );
    assert!(!out.status.success(), "should fail without result_snapshot");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("result_snapshot"), "got: {stderr}");
}

#[test]
fn session_done_works_without_registry_record() {
    // Sandbox-side use case: there's no local session record (the host
    // owns it), but `session done` should still emit the event so it
    // can ferry through the configured sinks (webhook / OTel).
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");

    let out = run(
        home.path(),
        cwd.path(),
        &["session", "done", "deadbeefcafe", "--status", "ok"],
    );
    assert_ok(&out, "session done stub");

    let events_path = home.path().join(".pillbox/global/events.jsonl");
    let body = fs::read_to_string(&events_path).expect("events.jsonl exists");
    let event: serde_json::Value = serde_json::from_str(body.trim()).expect("json");
    assert_eq!(event["event"], "session.completed");
    assert_eq!(event["session_id"], "deadbeefcafe");
    // Sandbox-side path has no local record: the session-derived
    // fields render as JSON null, NOT empty strings. Empty strings
    // would be a lie (the agent's remote is not literally ""); null
    // honestly says "we don't know this from here, correlate via
    // session_id against the host's session.started".
    assert!(event["remote"].is_null(), "got: {:?}", event["remote"]);
    assert!(event["backend"].is_null(), "got: {:?}", event["backend"]);
    assert!(event["agent_id"].is_null(), "got: {:?}", event["agent_id"]);
    assert!(
        event["started_at"].is_null(),
        "got: {:?}",
        event["started_at"]
    );
}

#[test]
fn detach_already_detached_is_noop() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    plant_session(home.path(), "abcdef123456", "cloud", None);

    let out = run(
        home.path(),
        cwd.path(),
        &["session", "detach", "abcdef123456"],
    );
    assert_ok(&out, "session detach");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("already detached"), "got: {stdout}");
}

#[test]
fn detach_refuses_reserved_pids() {
    // A hand-edited (or post-crash, recycled) session record with
    // attached_pid = 1 must not result in SIGTERM(1) — init/launchd
    // is never a pillbox we own. Same goes for pid 0 (process group
    // broadcast) and any value <= 1.
    for bad_pid in [0_i64, 1_i64] {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let out = run(home.path(), cwd.path(), &["init"]);
        assert_ok(&out, "init");
        plant_session_with_attached(home.path(), "abcdef111111", "cloud", None, Some(bad_pid));

        let out = run(
            home.path(),
            cwd.path(),
            &["session", "detach", "abcdef111111"],
        );
        assert!(
            !out.status.success(),
            "detach must refuse pid {bad_pid} (got success)"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("refusing to signal"),
            "pid {bad_pid}: stderr was: {stderr}"
        );
    }
}

#[test]
fn detach_clears_stamp_when_pid_is_gone() {
    // A previously-attached pillbox crashed before clearing
    // attached_pid. The stamped pid no longer exists. detach should
    // notice (kill probe → ESRCH), clear the stamp, and exit cleanly
    // without sending SIGTERM at whatever recycled pid lives there.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    // Pick a pid we're confident is unused on this host. 0x7FFFFFFE
    // sits below i32::MAX so it always fits libc::pid_t; it's also
    // outside the default pid_max on every platform we ship to.
    plant_session_with_attached(
        home.path(),
        "abcdef222222",
        "cloud",
        None,
        Some(0x7FFF_FFFE),
    );

    let out = run(
        home.path(),
        cwd.path(),
        &["session", "detach", "abcdef222222"],
    );
    assert_ok(&out, "session detach (dead pid)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no longer exists"), "got: {stderr}");

    // Stamp should now be cleared.
    let out = run(
        home.path(),
        cwd.path(),
        &["session", "info", "abcdef222222", "--json"],
    );
    assert_ok(&out, "session info post-detach");
    let parsed: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(parsed["session"]["attached_pid"], serde_json::Value::Null);
}

#[test]
fn rm_unknown_session_errors() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");

    let out = run(home.path(), cwd.path(), &["session", "rm", "deadbeefcafe"]);
    assert!(!out.status.success(), "should fail on unknown id");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no session matches"), "got: {stderr}");
}

#[test]
fn run_remote_detach_rejects_ssh_backend() {
    // PR 6 only supports detach for E2B remotes. Make sure the SSH
    // path emits the actionable not-yet-implemented error rather than
    // silently dropping to a half-detached run.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["new", "--name", "proj"]);
    assert_ok(&out, "new");
    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "add", "vps", "ssh://a@h"],
    );
    assert_ok(&out, "remote add ssh");

    let out = run(
        home.path(),
        cwd.path(),
        &["run", "--remote", "vps", "--detach"],
    );
    assert!(!out.status.success(), "ssh+detach should fail loudly");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ssh:// detached"), "got: {stderr}");
    assert!(stderr.contains("e2b://"), "got: {stderr}");
}

#[test]
fn detach_requires_remote() {
    // `--detach` without `--remote` is meaningless; clap enforces the
    // requires = "remote" constraint at parse time.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["run", "--detach"]);
    assert!(
        !out.status.success(),
        "--detach without --remote should fail"
    );
}

#[test]
fn label_only_meaningful_with_detach() {
    // `--label` is gated with `requires = "detach"` so the user gets a
    // loud clap error instead of a silently-discarded value. `--help`
    // short-circuits clap's requires-check, so we use it to assert the
    // flag is wired in, and a separate invocation without `--help` to
    // assert the constraint actually fires.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["run", "--label", "x", "--help"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--label"), "got: {stdout}");

    // Without `--detach`, clap should reject `--label`.
    let out = run(home.path(), cwd.path(), &["run", "--label", "x"]);
    assert!(
        !out.status.success(),
        "--label without --detach should fail"
    );
}
