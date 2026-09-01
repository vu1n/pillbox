//! End-to-end tests for `pillbox collect` — the fan-out result-collection verb.
//!
//! Drives the real binary against an isolated `$HOME` + a git-backed workspace,
//! covering what the unit tests can't: the CLI wiring, the absolutized manifest
//! paths, and `--as-refs` git-commit synthesis end to end. Pays ~5s × (rustic
//! init + two pushes) — the floor for exercising real rustic + git together.

mod common;

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

use common::{assert_ok, run};

/// Run `git -C dir <args>`, asserting success, returning trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// State dir of the single project pillbox under `home`.
fn project_state_dir(home: &Path) -> std::path::PathBuf {
    let projects = home.join(".pillbox/projects");
    std::fs::read_dir(&projects)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

/// Scrape the full snapshot handle from `pillbox push --json` stdout — tolerant
/// of compact-or-pretty JSON spacing, so the integration crate needs no
/// serde_json dep.
fn handle_from_push(stdout: &str) -> String {
    let after = &stdout[stdout.find("\"handle\"").expect("handle key") + "\"handle\"".len()..];
    let after = &after[after.find(':').unwrap() + 1..];
    let start = after.find('"').unwrap() + 1;
    let rest = &after[start..];
    rest[..rest.find('"').unwrap()].to_string()
}

#[test]
fn collect_emits_absolute_paths_and_as_refs_synthesizes_a_parented_ref() {
    if Command::new("git").arg("--version").output().is_err() {
        return; // CI without git
    }
    let home = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    let cwd = work.path();

    // 1. cwd is a git repo with a base commit.
    git(cwd, &["init", "-q"]);
    git(cwd, &["config", "user.email", "t@example"]);
    git(cwd, &["config", "user.name", "t"]);
    git(cwd, &["config", "commit.gpgsign", "false"]);
    std::fs::write(cwd.join("a.txt"), b"base").unwrap();
    git(cwd, &["add", "."]);
    git(cwd, &["commit", "-qm", "base"]);
    let base_sha = git(cwd, &["rev-parse", "HEAD"]);

    // 2. project pillbox + a base snapshot (its git_anchor == base_sha).
    assert_ok(
        &run(home.path(), cwd, &["new", "--name", "collectproj"]),
        "new",
    );
    let base_push = run(home.path(), cwd, &["push", "--json"]);
    assert_ok(&base_push, "push base");
    let base_handle = handle_from_push(&String::from_utf8_lossy(&base_push.stdout));

    // 3. the "agent" makes a change → result snapshot (HEAD unchanged, so the
    //    fork point's base_git_anchor is still base_sha).
    std::fs::write(cwd.join("b.txt"), b"result").unwrap();
    let res_push = run(home.path(), cwd, &["push", "--json"]);
    assert_ok(&res_push, "push result");
    let result_handle = handle_from_push(&String::from_utf8_lossy(&res_push.stdout));

    // 4. Hand-write a session record pointing at those snapshots. Sessions are
    //    normally minted by run/dispatch (which need a live backend); writing the
    //    TOML directly keeps this test backend-free. The schema is internal but
    //    stable — if it drifts, this canary should break.
    let sid = "aaaaaaaaaaaa";
    let sessions = project_state_dir(home.path()).join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{sid}.toml")),
        format!(
            "id = \"{sid}\"\n\
             backend = \"libkrun\"\n\
             sandbox_id = \"x\"\n\
             agent_id = \"claude\"\n\
             started_at = \"2026-01-01T00:00:00Z\"\n\
             base_snapshot = \"{base_handle}\"\n\
             result_snapshot = \"{result_handle}\"\n"
        ),
    )
    .unwrap();

    // 5. collect with a RELATIVE --to and --as-refs.
    let out = run(
        home.path(),
        cwd,
        &["collect", sid, "--to", "collected", "--as-refs", "--json"],
    );
    assert_ok(&out, "collect");
    let manifest = String::from_utf8_lossy(&out.stdout);

    // Absolutize: a relative `--to collected` must surface as an absolute,
    // cwd-rooted path (`/…/collected/<sid>`), never the bare relative form. The
    // leading `/` is the discriminator (and is robust to macOS `/private`
    // symlink prefixes, which an exact-path compare would trip on).
    assert!(
        manifest.contains(&format!("/collected/{sid}")),
        "manifest dir not absolutized: {manifest}"
    );
    // Lineage + the synthesized ref are reported.
    assert!(
        manifest.contains(&base_sha),
        "base_git_anchor (merge base) missing: {manifest}"
    );
    let refname = format!("refs/pillbox/collect/{sid}");
    assert!(
        manifest.contains(&refname),
        "ref missing from manifest: {manifest}"
    );

    // The result tree was rehydrated to disk (symlink-safe existence check).
    assert!(
        cwd.join("collected").join(sid).join("b.txt").is_file(),
        "result not rehydrated to disk"
    );

    // The ref exists, is parented on the base commit, and carries the worker's
    // new file alongside the base file — i.e. base→ref is exactly the change.
    assert_eq!(git(cwd, &["rev-parse", &format!("{refname}^")]), base_sha);
    let files = git(cwd, &["ls-tree", "-r", "--name-only", &refname]);
    assert!(files.lines().any(|l| l == "b.txt"), "{files}");
    assert!(files.lines().any(|l| l == "a.txt"), "{files}");
}
