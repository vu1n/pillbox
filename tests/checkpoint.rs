//! `pillbox checkpoint pack|unpack` through the real binary: the versioned
//! JSON contract Stratus parses, and the exit codes it branches on.

mod common;

use std::fs;
use std::process::Output;

use serde_json::Value;
use tempfile::TempDir;

use common::{assert_ok, run};

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout is one JSON object")
}

#[test]
fn pack_then_unpack_round_trips_with_versioned_json() {
    let home = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let ws = t.path().join("repo");
    let state = t.path().join("claude");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(ws.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    fs::create_dir_all(state.join("projects/p")).unwrap();
    fs::write(state.join("projects/p/s1.jsonl"), "{}\n").unwrap();
    fs::write(state.join(".credentials.json"), "{\"claudeAiOauth\":{}}\n").unwrap();
    let packet = t.path().join("out.packet");
    let s = |p: &std::path::Path| p.to_str().unwrap().to_string();

    let pack_args = [
        "checkpoint",
        "pack",
        "--workspace",
        &s(&ws),
        "--harness-state",
        &s(&state),
        "--harness",
        "claude_code",
        "--harness-session-id",
        "s1",
        "--transcript",
        "projects/p/s1.jsonl",
        "--out",
        &s(&packet),
        "--json",
    ];
    let out = run(home.path(), t.path(), &pack_args);
    assert_ok(&out, "pack");
    let packed = json(&out);
    assert_eq!(packed["version"], 1);
    assert_eq!(packed["format"], "pillbox.session-packet/1");
    assert_eq!(packed["bytes"], fs::metadata(&packet).unwrap().len());
    assert_eq!(packed["manifest"]["harness"], "claude_code");
    assert_eq!(packed["manifest"]["transcript"], "projects/p/s1.jsonl");
    assert_eq!(packed["excluded"][0]["path"], "harness/.credentials.json");
    let digest = packed["digest"].as_str().unwrap().to_string();
    assert!(digest.starts_with("sha256:") && digest.len() == 71);

    // Same tree, same digest.
    let again = run(home.path(), t.path(), &pack_args);
    assert_ok(&again, "pack again");
    assert_eq!(json(&again)["digest"], digest.as_str());

    let (ws2, state2) = (
        t.path().join("restore/repo"),
        t.path().join("restore/claude"),
    );
    let out = run(
        home.path(),
        t.path(),
        &[
            "checkpoint",
            "unpack",
            "--packet",
            &s(&packet),
            "--expect",
            &digest,
            "--workspace",
            &s(&ws2),
            "--harness-state",
            &s(&state2),
            "--json",
        ],
    );
    assert_ok(&out, "unpack");
    let unpacked = json(&out);
    assert_eq!(unpacked["version"], 1);
    assert_eq!(unpacked["digest"], digest.as_str());
    assert_eq!(unpacked["manifest"], packed["manifest"]);
    assert_eq!(
        fs::read_to_string(ws2.join("src/lib.rs")).unwrap(),
        "pub fn f() {}\n"
    );
    assert!(state2.join("projects/p/s1.jsonl").is_file());
    assert!(!state2.join(".credentials.json").exists());

    // A wrong digest is a runtime refusal (exit 1) and writes nothing.
    let wrong = format!("sha256:{}", "a".repeat(64));
    let target = t.path().join("never");
    let out = run(
        home.path(),
        t.path(),
        &[
            "checkpoint",
            "unpack",
            "--packet",
            &s(&packet),
            "--expect",
            &wrong,
            "--workspace",
            &s(&target),
            "--harness-state",
            &s(&state2),
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(!target.exists());
}

#[test]
fn checkpoint_is_hidden_from_help() {
    let home = TempDir::new().unwrap();
    let out = run(home.path(), home.path(), &["--help"]);
    assert_ok(&out, "help");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("checkpoint"));
    assert_ok(
        &run(home.path(), home.path(), &["checkpoint", "pack", "--help"]),
        "checkpoint pack --help",
    );
}

#[test]
fn unpack_restores_directory_modes_under_a_narrow_umask() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    let home = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let ws = t.path().join("repo");
    fs::create_dir_all(ws.join("src/inner")).unwrap();
    fs::write(ws.join("src/inner/lib.rs"), "pub fn f() {}\n").unwrap();
    let packet = t.path().join("out.packet");
    let s = |p: &std::path::Path| p.to_str().unwrap().to_string();
    let out = run(
        home.path(),
        t.path(),
        &[
            "checkpoint",
            "pack",
            "--workspace",
            &s(&ws),
            "--harness",
            "claude_code",
            "--out",
            &s(&packet),
            "--json",
        ],
    );
    assert_ok(&out, "pack");
    let digest = json(&out)["digest"].as_str().unwrap().to_string();

    let restored = t.path().join("restore");
    let mut cmd = std::process::Command::new(common::pillbox_bin());
    cmd.env("HOME", home.path()).current_dir(t.path()).args([
        "checkpoint",
        "unpack",
        "--packet",
        &s(&packet),
        "--expect",
        &digest,
        "--workspace",
        &s(&restored),
        "--json",
    ]);
    // SAFETY: umask is async-signal-safe and touches only the child.
    unsafe {
        cmd.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let out = cmd.output().unwrap();
    assert_ok(&out, "unpack");
    let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&restored.join("src")), 0o755);
    assert_eq!(mode(&restored.join("src/inner")), 0o755);
    assert_eq!(mode(&restored.join("src/inner/lib.rs")), 0o644);
}

#[test]
fn pack_refuses_an_output_inside_a_packed_tree() {
    let home = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let ws = t.path().join("repo");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("a.txt"), "a\n").unwrap();
    let s = |p: &std::path::Path| p.to_str().unwrap().to_string();
    let out = run(
        home.path(),
        t.path(),
        &[
            "checkpoint",
            "pack",
            "--workspace",
            &s(&ws),
            "--harness",
            "claude_code",
            "--out",
            &s(&ws.join("out.packet")),
            "--json",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(!ws.join("out.packet").exists());
}
