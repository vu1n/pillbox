//! The standalone `pillbox-packet` binary: the versioned JSON callers parse,
//! the `--name=value` flag form Stratus passes, and the exit codes callers
//! branch on (1 = refused packet, 2 = bad flags or a non-empty target).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn packet(args: &[String]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pillbox-packet"))
        .args(args)
        .output()
        .unwrap()
}

fn json(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout is one JSON object")
}

fn arg(flag: &str, path: &Path) -> String {
    format!("--{flag}={}", path.display())
}

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Exit `code` with a `pillbox-packet: <op> failed. …` line on stderr.
fn assert_fails(out: &Output, code: i32, op: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(code), "{stderr}");
    assert!(
        stderr.starts_with(&format!("pillbox-packet: {op} failed. ")),
        "{stderr}"
    );
}

#[test]
fn pack_and_unpack_print_versioned_json_and_exit_by_category() {
    let t = TempDir::new().unwrap();
    let (ws, hs) = (t.path().join("ws"), t.path().join("hs"));
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(ws.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    fs::create_dir_all(hs.join("projects/p")).unwrap();
    fs::write(hs.join("projects/p/s1.jsonl"), "{}\n").unwrap();
    fs::write(hs.join(".credentials.json"), "{}\n").unwrap();
    let out = t.path().join("out.packet");

    let mut pack = vec![
        arg("workspace", &ws),
        arg("harness-state", &hs),
        "--harness=claude_code".into(),
        "--harness-session-id=s1".into(),
        "--transcript=projects/p/s1.jsonl".into(),
        arg("out", &out),
    ];
    pack.insert(0, "pack".into());
    pack.push("--json".into());
    let packed = json(&packet(&pack));
    let keys: Vec<&str> = packed
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["bytes", "digest", "excluded", "format", "manifest", "version"]
    );
    assert_eq!(packed["version"], 1);
    assert_eq!(packed["format"], "pillbox.session-packet/1");
    assert_eq!(packed["bytes"], fs::metadata(&out).unwrap().len());
    assert_eq!(packed["manifest"]["harness_session_id"], "s1");
    assert_eq!(packed["excluded"][0]["path"], "harness/.credentials.json");
    assert_eq!(packed["excluded"][0]["reason"], "credential");
    let digest = packed["digest"].as_str().unwrap().to_string();

    let unpack = |ws: &Path, expect: &str| {
        let mut a = args(&["unpack", "--packet"]);
        a.push(out.display().to_string());
        a.push(format!("--expect={expect}"));
        a.push(arg("workspace", ws));
        a.push(arg("harness-state", &t.path().join("restored-hs")));
        a.push("--json".into());
        packet(&a)
    };
    let restored = t.path().join("restored");
    let unpacked = json(&unpack(&restored, &digest));
    assert_eq!(unpacked["version"], 1);
    assert_eq!(unpacked["format"], "pillbox.session-packet/1");
    assert_eq!(unpacked["digest"], digest.as_str());
    assert_eq!(unpacked["manifest"], packed["manifest"]);
    assert_eq!(
        fs::read(restored.join("src/lib.rs")).unwrap(),
        b"pub fn f() {}\n"
    );

    // Refused packet: exit 1.
    let wrong = format!("sha256:{}", "0".repeat(64));
    assert_fails(&unpack(&t.path().join("w1"), &wrong), 1, "unpack");
    // Non-empty target: exit 2.
    assert_fails(&unpack(&restored, &digest), 2, "unpack");
    // Malformed --expect: exit 2.
    assert_fails(&unpack(&t.path().join("w2"), "sha256:abc"), 2, "unpack");
    // Missing --workspace directory: exit 2.
    let mut bad = args(&["pack", "--harness=x"]);
    bad.push(arg("workspace", &t.path().join("nope")));
    bad.push(arg("out", &t.path().join("x.packet")));
    assert_fails(&packet(&bad), 2, "pack");
}
