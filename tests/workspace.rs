//! End-to-end tests for the v0.6 PR 3 workspace surface.
//!
//! These exercise the real `pillbox` binary against an isolated `$HOME`
//! and a local-backend rustic repo. The integration suite focuses on
//! flows the unit tests can't easily cover: cwd-as-workspace, the
//! command-line plumbing for `push` / `pull` / `snapshot list / show /
//! rm` / `workspace rekey`, and the `--from-git` inflow.
//!
//! Each test that does a `pillbox new` pays ~5s for rustic's scrypt
//! key-derivation pass. That's the floor of running real rustic at
//! all; we don't bypass it here so the integration tests prove the
//! repo can be re-opened cleanly.

mod common;

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

use common::{assert_ok, pillbox_bin, run};

#[test]
fn new_initializes_rustic_repo_for_local_backend() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["new", "--name", "alpha"]),
        "new",
    );

    // Find the state dir; the rustic repo lives at `<state>/repo/`.
    let projects = home.path().join(".pillbox/projects");
    let entry = std::fs::read_dir(&projects)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let state = entry.path();
    assert!(state.join("repo/config").is_file(), "rustic config missing");
    let pw = state.join("repo-password");
    assert!(pw.is_file(), "password file missing");
    // 0600.
    let mode = std::fs::metadata(&pw).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(mode.mode() & 0o777, 0o600);
}

#[test]
fn pillbox_toml_records_local_backend() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["new", "--name", "beta"]),
        "new",
    );
    let body = std::fs::read_to_string(cwd.path().join("pillbox.toml")).unwrap();
    assert!(body.contains("[workspace]"), "got: {body}");
    assert!(body.contains("backend = \"local\""), "got: {body}");
}

#[test]
fn new_with_s3_backend_persists_config_without_calling_s3() {
    // Provide env vars so `pillbox new` doesn't fail on missing
    // creds — the test stub backend isn't actually contacted (init
    // would try, so we only check that the descriptor is written
    // BEFORE init runs). To make this test fast and not require live
    // S3, set PILLBOX_SKIP_WORKSPACE_INIT.
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = Command::new(pillbox_bin())
        .env("HOME", home.path())
        .env("PILLBOX_SKIP_WORKSPACE_INIT", "1")
        .env("R2_ACCESS_KEY", "test-access")
        .env("R2_SECRET_KEY", "test-secret")
        .current_dir(cwd.path())
        .args([
            "new",
            "--name",
            "s3-proj",
            "--workspace-backend",
            "s3",
            "--bucket",
            "my-bucket",
            "--endpoint",
            "https://acct.r2.cloudflarestorage.com",
            "--region",
            "auto",
            "--prefix",
            "pillbox/",
            "--access-key-env",
            "R2_ACCESS_KEY",
            "--secret-key-env",
            "R2_SECRET_KEY",
        ])
        .output()
        .unwrap();
    assert_ok(&out, "new s3");

    let body = std::fs::read_to_string(cwd.path().join("pillbox.toml")).unwrap();
    assert!(body.contains("backend = \"s3\""), "got: {body}");
    assert!(body.contains("bucket = \"my-bucket\""), "got: {body}");
    assert!(body.contains("endpoint = \"https://acct"), "got: {body}");
    assert!(
        body.contains("access_key_env = \"R2_ACCESS_KEY\""),
        "got: {body}"
    );
}

#[test]
fn s3_backend_rejects_missing_required_flags() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(
        home.path(),
        cwd.path(),
        &["new", "--name", "x", "--workspace-backend", "s3"],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--bucket") || stderr.contains("bucket"),
        "got: {stderr}"
    );
}

#[test]
fn local_backend_rejects_stray_s3_flag() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(
        home.path(),
        cwd.path(),
        &["new", "--name", "x", "--bucket", "boom"],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--bucket") || stderr.contains("bucket"),
        "got: {stderr}"
    );
}

#[test]
fn push_then_snapshot_list_returns_one() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["new", "--name", "p1"]),
        "new",
    );
    std::fs::write(cwd.path().join("file.txt"), b"hello").unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["push", "--tag", "v1"]),
        "push",
    );

    let out = run(home.path(), cwd.path(), &["snapshot", "list", "--json"]);
    assert_ok(&out, "snapshot list");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let snaps = v["snapshots"].as_array().expect("snapshots array");
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0]["tag"], "v1");
    let handle = snaps[0]["handle"].as_str().unwrap();
    assert_eq!(handle.len(), 64);
}

#[test]
fn push_pull_round_trips_the_workspace() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["new", "--name", "rt"]),
        "new",
    );
    std::fs::write(cwd.path().join("greeting.txt"), b"original").unwrap();
    assert_ok(&run(home.path(), cwd.path(), &["push"]), "push");

    // Mutate the file, then pull from latest snapshot. The file
    // should be restored back to "original" — the restore writes
    // under the absolute source path inside cwd, mimicking restic.
    std::fs::write(cwd.path().join("greeting.txt"), b"mutated").unwrap();
    assert_ok(&run(home.path(), cwd.path(), &["pull"]), "pull");

    // Locate the restored greeting.txt anywhere in cwd.
    let mut stack = vec![cwd.path().to_path_buf()];
    let mut found: Option<PathBuf> = None;
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).unwrap().flatten() {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .map(|n| n == "greeting.txt")
                .unwrap_or(false)
            {
                // Skip the one we mutated at the workspace root — that
                // one is the input, not the restore target.
                if path.parent() != Some(cwd.path()) {
                    found = Some(path);
                }
            }
        }
    }
    let restored = found.expect("restored greeting.txt");
    let body = std::fs::read(&restored).unwrap();
    assert_eq!(body, b"original");
}

#[test]
fn workspace_rekey_rotates_password() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["new", "--name", "rk"]),
        "new",
    );

    let projects = home.path().join(".pillbox/projects");
    let entry = std::fs::read_dir(&projects)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let pw_before = std::fs::read_to_string(entry.path().join("repo-password")).unwrap();

    assert_ok(
        &run(home.path(), cwd.path(), &["workspace", "rekey"]),
        "rekey",
    );
    let pw_after = std::fs::read_to_string(entry.path().join("repo-password")).unwrap();
    assert_ne!(pw_before, pw_after);
}

#[test]
fn from_git_clones_into_cwd() {
    // Build a local bare repo with one commit, then `pillbox new
    // --from-git file://...` clones it into an empty cwd.
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return; // Skip if git isn't installed.
    }
    let src = TempDir::new().unwrap();
    let run_git = |dir: &std::path::Path, args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
    };
    run_git(src.path(), &["init", "-q"]);
    run_git(src.path(), &["config", "user.email", "t@example"]);
    run_git(src.path(), &["config", "user.name", "t"]);
    run_git(src.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(src.path().join("readme.txt"), b"hello git").unwrap();
    run_git(src.path(), &["add", "."]);
    run_git(src.path(), &["commit", "-q", "-m", "init"]);

    let bare_dir = TempDir::new().unwrap();
    let bare = bare_dir.path().join("repo.git");
    let out = std::process::Command::new("git")
        .args(["clone", "--bare"])
        .arg(src.path())
        .arg(&bare)
        .output()
        .unwrap();
    assert!(out.status.success(), "bare clone failed");
    let url = format!("file://{}", bare.display());

    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(
        home.path(),
        cwd.path(),
        &["new", "--name", "cloned", "--from-git", &url],
    );
    assert_ok(&out, "new --from-git");

    // The cloned tree should have readme.txt.
    assert!(
        cwd.path().join("readme.txt").is_file(),
        "clone missing readme"
    );
    // And a .git directory.
    assert!(cwd.path().join(".git").is_dir(), "clone missing .git");
}

#[test]
fn from_git_refuses_non_empty_cwd() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    std::fs::write(cwd.path().join("preexisting.txt"), b"x").unwrap();
    let out = run(
        home.path(),
        cwd.path(),
        &[
            "new",
            "--name",
            "x",
            "--from-git",
            "file:///does/not/matter",
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("empty") || stderr.contains("not empty"),
        "got: {stderr}"
    );
}

#[test]
fn snapshot_show_supports_prefix() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["new", "--name", "px"]),
        "new",
    );
    std::fs::write(cwd.path().join("a"), b"x").unwrap();
    assert_ok(&run(home.path(), cwd.path(), &["push"]), "push");
    // Get the full handle, then call show with an 8-char prefix.
    let out = run(home.path(), cwd.path(), &["snapshot", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let full = v["snapshots"][0]["handle"].as_str().unwrap().to_string();
    let prefix = &full[..8];
    let out = run(
        home.path(),
        cwd.path(),
        &["snapshot", "show", prefix, "--json"],
    );
    assert_ok(&out, "snapshot show prefix");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["snapshot"]["handle"], full);
}

#[test]
fn snapshot_rm_removes_a_snapshot() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["new", "--name", "del"]),
        "new",
    );
    std::fs::write(cwd.path().join("a"), b"x").unwrap();
    assert_ok(&run(home.path(), cwd.path(), &["push"]), "push 1");
    std::fs::write(cwd.path().join("b"), b"y").unwrap();
    assert_ok(&run(home.path(), cwd.path(), &["push"]), "push 2");

    let out = run(home.path(), cwd.path(), &["snapshot", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let snaps = v["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 2);
    let first = snaps[0]["handle"].as_str().unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["snapshot", "rm", first]),
        "rm",
    );

    let out = run(home.path(), cwd.path(), &["snapshot", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["snapshots"].as_array().unwrap().len(), 1);
}

#[test]
fn snapshot_list_empty_emits_helpful_message() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), cwd.path(), &["new", "--name", "e"]),
        "new",
    );
    let out = run(home.path(), cwd.path(), &["snapshot", "list"]);
    assert_ok(&out, "snapshot list empty");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no snapshots"), "got: {stdout}");
}

#[test]
fn workspace_command_requires_a_project_pillbox() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    // No pillbox.toml; we resolve to global, which has no workspace.
    let out = run(home.path(), cwd.path(), &["push"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("workspace") || stderr.contains("global"),
        "got: {stderr}"
    );
}
