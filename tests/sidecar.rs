//! End-to-end test for `pillbox sidecar`.
//!
//! v0.6 PR 1: the sidecar boots the existing vault `Server` standalone,
//! prints its listen addr + ca cert path on stdout, and blocks until
//! SIGTERM. This test spawns the real binary, reads the first JSON
//! line, asserts the listen_addr is parseable + the pid is the child
//! pid, then sends SIGTERM and expects a clean exit within a reasonable
//! window.
//!
//! We point `$HOME` at a tempdir so the vault CA + state files don't
//! pollute the dev machine.

mod common;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::pillbox_bin;

#[test]
fn sidecar_prints_listen_addr_and_shuts_down_on_sigterm() {
    // Use `tempfile::TempDir` (already a pillbox dep) so a panic mid-test
    // doesn't leak the dir — the Drop impl cleans up regardless.
    let home = tempfile::Builder::new()
        .prefix("pillbox-sidecar-test-")
        .tempdir()
        .expect("create tempdir");

    let mut child = Command::new(pillbox_bin())
        .arg("sidecar")
        .arg("--json")
        .env("HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pillbox sidecar");

    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    // Reading the first line ensures the proxy has bound — `sidecar_run`
    // flushes after start.
    reader.read_line(&mut line).expect("read sidecar JSON line");

    let value: serde_json::Value = serde_json::from_str(line.trim()).expect("parse sidecar JSON");
    assert_eq!(value["version"], 1);
    let addr_str = value["listen_addr"]
        .as_str()
        .expect("listen_addr is a string");
    let addr: SocketAddr = addr_str.parse().expect("listen_addr parses");
    assert!(addr.port() > 0, "expected bound port, got {addr}");
    let ca = value["ca_cert_path"]
        .as_str()
        .expect("ca_cert_path is a string");
    assert!(
        std::path::Path::new(ca).exists(),
        "ca cert path does not exist: {ca}"
    );
    let pid = value["pid"].as_u64().expect("pid is a number");
    assert_eq!(pid, child.id() as u64);

    // Send SIGTERM. unix::process::ExitStatusExt is the portable way,
    // but for tests we just shell out — pillbox sidecar is unix-only
    // anyway (Docker requirement).
    let kill_status = Command::new("kill")
        .arg(child.id().to_string())
        .status()
        .expect("kill");
    assert!(kill_status.success());

    // Wait with a generous deadline. Local typical: <100ms.
    let deadline = Instant::now() + Duration::from_secs(5);
    let exit = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("sidecar did not exit within 5s of SIGTERM");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert!(exit.success(), "sidecar exited non-zero: {exit:?}");
    // `home` drops here — tempfile cleans up the tempdir.
}
