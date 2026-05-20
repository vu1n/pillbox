//! End-to-end tests for the v0.6 pillbox-as-bundle CLI.
//!
//! These run the real binary against an isolated `$HOME` and cover the
//! command-surface boundaries the unit tests can't easily exercise:
//! `pillbox init / new / list / info / rm`, the cwd discovery + `--pillbox`
//! flag, secret inheritance, and the legacy-layout migration error.

mod common;

use std::process::Command;

use tempfile::TempDir;

use common::{assert_ok, pillbox_bin, run};

#[test]
fn init_creates_global_pillbox() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let out = run(home.path(), cwd.path(), &["init"]);
    assert_ok(&out, "init");
    let global_dir = home.path().join(".pillbox/global");
    assert!(global_dir.is_dir(), "global dir not created");
}

#[test]
fn new_writes_descriptor_and_state_dir() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(
        home.path(),
        cwd.path(),
        &["new", "--name", "alpha", "--agent", "claude"],
    );
    assert_ok(&out, "new");

    let descriptor = cwd.path().join("pillbox.toml");
    assert!(descriptor.is_file());
    let body = std::fs::read_to_string(&descriptor).unwrap();
    assert!(body.contains("name = \"alpha\""), "got: {body}");
    assert!(body.contains("agent = \"claude\""), "got: {body}");

    // State dir is somewhere under ~/.pillbox/projects/.
    let projects = home.path().join(".pillbox/projects");
    let entries: Vec<_> = std::fs::read_dir(&projects)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1, "expected one project state dir");
    let state = entries[0].path();
    assert!(state.join("meta.json").is_file());
}

#[test]
fn new_refuses_when_descriptor_exists() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    std::fs::write(cwd.path().join("pillbox.toml"), "name = \"existing\"\n").unwrap();

    let out = run(home.path(), cwd.path(), &["new"]);
    assert!(!out.status.success(), "should have failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected 'already exists' message; got: {stderr}"
    );
}

#[test]
fn discovery_walks_up_to_find_descriptor() {
    let home = TempDir::new().unwrap();
    let project_root = TempDir::new().unwrap();

    // Create the project pillbox at the root.
    let out = run(home.path(), project_root.path(), &["new", "--name", "beta"]);
    assert_ok(&out, "new beta");

    // From a nested directory, info should still find `beta`.
    let nested = project_root.path().join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    let out = run(home.path(), &nested, &["info", "--json"]);
    assert_ok(&out, "info from nested");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json parse");
    assert_eq!(v["pillbox"]["name"], "beta");
    assert_eq!(v["pillbox"]["scope"], "project");
}

#[test]
fn discovery_falls_back_to_global_with_no_descriptor() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    let out = run(home.path(), cwd.path(), &["info", "--json"]);
    assert_ok(&out, "info global fallback");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["pillbox"]["scope"], "global");
    assert_eq!(v["from_pillbox_toml"], false);
}

#[test]
fn pillbox_flag_overrides_discovery() {
    let home = TempDir::new().unwrap();
    let proj_a = TempDir::new().unwrap();
    let proj_b = TempDir::new().unwrap();

    assert_ok(
        &run(home.path(), proj_a.path(), &["new", "--name", "aproj"]),
        "new aproj",
    );
    assert_ok(
        &run(home.path(), proj_b.path(), &["new", "--name", "bproj"]),
        "new bproj",
    );

    // From cwd=proj_a (which would discover aproj), `--pillbox bproj`
    // should select bproj.
    let out = run(
        home.path(),
        proj_a.path(),
        &["--pillbox", "bproj", "info", "--json"],
    );
    assert_ok(&out, "info --pillbox bproj");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pillbox"]["name"], "bproj");
}

#[test]
fn list_shows_global_and_projects() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    assert_ok(&run(home.path(), proj.path(), &["init"]), "init");
    assert_ok(
        &run(home.path(), proj.path(), &["new", "--name", "gamma"]),
        "new gamma",
    );

    let out = run(home.path(), proj.path(), &["list", "--json"]);
    assert_ok(&out, "list");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entries = v["pillboxes"].as_array().expect("pillboxes is array");
    assert!(entries.iter().any(|e| e["scope"] == "global"));
    assert!(entries
        .iter()
        .any(|e| e["scope"] == "project" && e["name"] == "gamma"));
}

#[test]
fn rm_deletes_project_pillbox() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), proj.path(), &["new", "--name", "delta"]),
        "new delta",
    );

    // Find the state dir.
    let projects = home.path().join(".pillbox/projects");
    let entry = std::fs::read_dir(&projects)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let state_dir = entry.path();
    assert!(state_dir.is_dir());

    assert_ok(&run(home.path(), proj.path(), &["rm", "delta"]), "rm");
    assert!(!state_dir.exists(), "state dir should have been removed");
}

#[test]
fn rm_refuses_global() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    assert_ok(&run(home.path(), cwd.path(), &["init"]), "init");

    let out = run(home.path(), cwd.path(), &["rm", "global"]);
    assert!(!out.status.success(), "should refuse");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing") || stderr.contains("global"),
        "got: {stderr}"
    );
}

#[test]
fn legacy_layout_blocks_init() {
    let home = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();

    // Simulate a v0.5 install.
    std::fs::create_dir_all(home.path().join(".pillbox/data/claude")).unwrap();
    std::fs::create_dir_all(home.path().join(".pillbox/secrets")).unwrap();

    let out = run(home.path(), cwd.path(), &["init"]);
    assert!(!out.status.success(), "should refuse with legacy layout");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("v0.5") || stderr.contains("legacy"),
        "got: {stderr}"
    );
}

#[test]
fn secret_inheritance_global_to_project() {
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();

    assert_ok(&run(home.path(), proj.path(), &["init"]), "init");
    assert_ok(
        &run(home.path(), proj.path(), &["new", "--name", "epsilon"]),
        "new epsilon",
    );

    // Add a secret to global from inside the project (using --global).
    let out = Command::new(pillbox_bin())
        .env("HOME", home.path())
        .env("__PB_VAL", "global-value")
        .current_dir(proj.path())
        .args([
            "secret",
            "add",
            "SHARED",
            "--from-env",
            "__PB_VAL",
            "--global",
        ])
        .output()
        .unwrap();
    assert_ok(&out, "add SHARED --global");

    // List from the project: SHARED appears as global-scoped.
    let out = run(home.path(), proj.path(), &["secret", "list", "--json"]);
    assert_ok(&out, "list");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let s = v["secrets"].as_array().unwrap();
    let shared = s.iter().find(|e| e["name"] == "SHARED").unwrap();
    assert_eq!(shared["scope"], "global");

    // Override in project — same name, different value.
    let out = Command::new(pillbox_bin())
        .env("HOME", home.path())
        .env("__PB_VAL2", "project-value")
        .current_dir(proj.path())
        .args(["secret", "add", "SHARED", "--from-env", "__PB_VAL2"])
        .output()
        .unwrap();
    assert_ok(&out, "add SHARED to project");

    let out = run(home.path(), proj.path(), &["secret", "list", "--json"]);
    assert_ok(&out, "list 2");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let s = v["secrets"].as_array().unwrap();
    let shared = s.iter().find(|e| e["name"] == "SHARED").unwrap();
    // Project shadows global.
    assert_eq!(shared["scope"], "epsilon");
}

#[test]
fn auth_list_reads_from_global_scope() {
    // Auth defaults to GLOBAL even from inside a project pillbox.
    // We can't fully exercise `auth login` (needs Docker), but `auth list`
    // exits cleanly and reports zero entries, while the path it prints
    // should reference the global pillbox's auth dir.
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    assert_ok(
        &run(home.path(), proj.path(), &["new", "--name", "zeta"]),
        "new zeta",
    );

    let out = run(home.path(), proj.path(), &["auth", "list", "--json"]);
    assert_ok(&out, "auth list");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    for agent in v["agents"].as_array().unwrap() {
        let home_str = agent["home"].as_str().unwrap();
        assert!(
            home_str.contains(".pillbox/global/auth"),
            "auth home should be under global: {home_str}",
        );
    }
}

#[test]
fn pillbox_toml_agent_field_picks_default() {
    // Without --agent, `pillbox run` should pick the descriptor's `agent`.
    // We can't actually launch Docker, but the "no stored credentials"
    // error mentions the resolved agent, which is enough to assert.
    let home = TempDir::new().unwrap();
    let proj = TempDir::new().unwrap();
    assert_ok(
        &run(
            home.path(),
            proj.path(),
            &["new", "--name", "eta", "--agent", "codex"],
        ),
        "new eta",
    );

    let out = run(home.path(), proj.path(), &["run"]);
    // Will fail — either no Docker or no credentials. Either way the
    // chosen agent must be codex.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("codex") || stderr.contains("Docker") || stderr.contains("docker"),
        "expected codex or docker reference; got: {stderr}"
    );
    assert!(
        !stderr.contains("claude"),
        "unexpected claude mention: {stderr}"
    );
}
