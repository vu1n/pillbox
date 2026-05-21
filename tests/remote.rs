//! End-to-end tests for `pillbox remote {add,list,rm,info}` and the
//! workspace-handoff guard on `pillbox run --remote`.
//!
//! Live SSH is out of scope for v0.6 PR 4 — we exercise the registry
//! surface and the local-side error paths via the real binary, but the
//! actual openssh connection is only covered when a user wires up a
//! VPS by hand. See `tests/common/mod.rs` for the helper shape.

mod common;

use tempfile::TempDir;

use common::{assert_ok, run};

#[test]
fn add_and_list_round_trip() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");

    // Positional URL — canonical form (matches `git remote add`).
    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "add", "my-vps", "ssh://alice@host.example"],
    );
    assert_ok(&out, "remote add");

    let out = run(home.path(), cwd.path(), &["remote", "list", "--json"]);
    assert_ok(&out, "remote list");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"version\":1"), "got: {stdout}");
    assert!(stdout.contains("my-vps"), "got: {stdout}");
    assert!(stdout.contains("ssh://alice@host.example"), "got: {stdout}");
}

#[test]
fn add_rejects_unsupported_scheme() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "add", "bad", "http://nope.example"],
    );
    assert!(!out.status.success(), "expected failure");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Both supported schemes should be mentioned so the user knows
    // their options without re-reading docs.
    assert!(stderr.contains("ssh://"), "got: {stderr}");
    assert!(stderr.contains("e2b://"), "got: {stderr}");
    // Usage error → exit code 2.
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn add_accepts_e2b_url() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");

    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "add", "prod-e2b", "e2b://pillbox-runner-template"],
    );
    assert_ok(&out, "remote add e2b");

    let out = run(home.path(), cwd.path(), &["remote", "list", "--json"]);
    assert_ok(&out, "remote list");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("prod-e2b"), "got: {stdout}");
    assert!(
        stdout.contains("e2b://pillbox-runner-template"),
        "got: {stdout}"
    );
}

#[test]
fn info_emits_kind_for_e2b() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "add", "cloud", "e2b://my-template"],
    );
    assert_ok(&out, "remote add");

    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "info", "cloud", "--json"],
    );
    assert_ok(&out, "remote info");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json parse");
    assert_eq!(parsed["remote"]["kind"], serde_json::json!("e2b"));
    assert_eq!(
        parsed["remote"]["url"],
        serde_json::json!("e2b://my-template")
    );
}

#[test]
fn add_rejects_empty_e2b_template() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["remote", "add", "bad", "e2b://"]);
    assert!(!out.status.success(), "expected failure");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing template id"), "got: {stderr}");
}

#[test]
fn info_emits_stable_json() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    // Keep one test exercising the hidden `--url` alias so we don't
    // accidentally regress it. The rest of the suite uses positional.
    let out = run(
        home.path(),
        cwd.path(),
        &[
            "remote",
            "add",
            "vps",
            "--url",
            "ssh://bob@10.0.0.1:2222",
            "--agent",
            "claude",
        ],
    );
    assert_ok(&out, "remote add (--url alias)");

    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "info", "vps", "--json"],
    );
    assert_ok(&out, "remote info");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json parse");
    assert_eq!(parsed["version"], serde_json::json!(1));
    assert_eq!(parsed["remote"]["name"], serde_json::json!("vps"));
    assert_eq!(
        parsed["remote"]["url"],
        serde_json::json!("ssh://bob@10.0.0.1:2222")
    );
    assert_eq!(
        parsed["remote"]["default_agent"],
        serde_json::json!("claude")
    );
}

#[test]
fn rm_removes_and_is_idempotent() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");

    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "add", "vps", "ssh://a@h"],
    );
    assert_ok(&out, "add");

    let out = run(home.path(), cwd.path(), &["remote", "rm", "vps"]);
    assert_ok(&out, "rm");

    // Second rm should still exit zero (idempotent).
    let out = run(home.path(), cwd.path(), &["remote", "rm", "vps"]);
    assert_ok(&out, "rm again");
}

#[test]
fn project_inherits_global_remote() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");

    // Register globally first.
    let out = run(
        home.path(),
        cwd.path(),
        &[
            "remote",
            "add",
            "shared",
            "--url",
            "ssh://g@global.example",
            "--global",
        ],
    );
    assert_ok(&out, "global add");

    // Create a project pillbox, then look up `shared` from inside it.
    let out = run(home.path(), cwd.path(), &["new", "--name", "proj"]);
    assert_ok(&out, "new");

    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "info", "shared", "--json"],
    );
    assert_ok(&out, "remote info");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("global"), "got: {stdout}");
    assert!(stdout.contains("ssh://g@global.example"), "got: {stdout}");
}

#[test]
fn run_remote_refuses_when_remote_unknown() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    // Create a project pillbox so we have somewhere to launch from.
    let out = run(home.path(), cwd.path(), &["new", "--name", "proj"]);
    assert_ok(&out, "new");

    let out = run(
        home.path(),
        cwd.path(),
        &["run", "--remote", "no-such-remote"],
    );
    assert!(!out.status.success(), "should have failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no-such-remote") || stderr.contains("not found"),
        "got: {stderr}"
    );
}

#[test]
fn run_remote_e2b_refuses_local_workspace() {
    // E2B backend has the same S3-backend requirement as the SSH path:
    // the in-sandbox pillbox needs to restore the workspace from a
    // shared bucket, not from a local rustic repo the helper can't see.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["new", "--name", "proj"]);
    assert_ok(&out, "new");
    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "add", "cloud", "e2b://my-template"],
    );
    assert_ok(&out, "remote add");

    let out = run(home.path(), cwd.path(), &["run", "--remote", "cloud"]);
    assert!(!out.status.success(), "should have failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("S3") || stderr.contains("s3"),
        "expected workspace-backend hint; got: {stderr}"
    );
    // Even the gating message should be E2B-specific (not say "SSH").
    assert!(stderr.contains("e2b"), "got: {stderr}");
}

#[test]
fn run_remote_rejects_vault_stdin_combo() {
    // `--vault-stdin` is the internal "remote-side" mode of the wire
    // protocol; pairing it with `--remote NAME` makes no semantic sense
    // and would be a footgun if it ever did anything other than fail.
    // clap's `conflicts_with` enforces the rejection at parse time.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["new", "--name", "proj"]);
    assert_ok(&out, "new");
    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "add", "cloud", "e2b://t"],
    );
    assert_ok(&out, "remote add");

    let out = run(
        home.path(),
        cwd.path(),
        &["run", "--remote", "cloud", "--vault-stdin"],
    );
    assert!(!out.status.success(), "should have failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // clap's wording for `conflicts_with`. Don't pin the exact phrase;
    // grep for the two flag names so the test survives clap upgrades.
    assert!(
        (stderr.contains("--remote") || stderr.contains("remote"))
            && (stderr.contains("--vault-stdin") || stderr.contains("vault-stdin")),
        "expected conflict diagnostic mentioning both flags; got: {stderr}"
    );
}

#[test]
fn run_remote_refuses_local_workspace_in_pr4() {
    // PR 4 ships S3-only. A pillbox with the default `local` backend
    // must error out with the "PR 4.1 will add tarball transport" hint
    // before we ever reach the SSH step.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["new", "--name", "proj"]);
    assert_ok(&out, "new");
    let out = run(
        home.path(),
        cwd.path(),
        &["remote", "add", "vps", "--url", "ssh://a@h"],
    );
    assert_ok(&out, "remote add");

    let out = run(home.path(), cwd.path(), &["run", "--remote", "vps"]);
    assert!(!out.status.success(), "should have failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("S3") || stderr.contains("s3"),
        "expected workspace-backend hint; got: {stderr}"
    );
}
