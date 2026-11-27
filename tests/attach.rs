//! Integration tests for the attach transport (phase 2). They drive the
//! real `pillbox` binary and assert the framed snapshot/stream carries the
//! agent's output. **All `#[ignore]`d** — they spawn real subprocesses and are
//! timing-sensitive, so they flake when the full `cargo test` suite saturates
//! the CPU; run them deliberately on an idle machine with
//! `cargo test --test attach -- --ignored`. They do not gate routine `cargo test`.
//!
//!   - direct: a client on the pty-host's unix socket.
//!
//! The frame reader is hand-rolled (not the crate's `Frame`) so these also
//! pin the wire format from the outside: `[type:u8][len:u32 BE][payload]`.

mod common;

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::sync::mpsc;
use std::time::Duration;

const MARKER: &[u8] = b"PILLBOXPHASE2OK";
const AGENT_CMD: &str = "printf 'PILLBOXPHASE2OK\\n'; sleep 10";

fn unique_sock() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("pb-attach-{}-{}.sock", std::process::id(), nanos))
}

/// Read one frame: 5-byte header + body. `None` on EOF/short read.
fn read_typed<R: Read>(r: &mut R) -> Option<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr).ok()?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).ok()?;
    Some((hdr[0], payload))
}

fn read_frame<R: Read>(r: &mut R) -> Option<Vec<u8>> {
    read_typed(r).map(|(_, p)| p)
}

/// Accumulate frame payloads from `r` until the marker appears (or the
/// stream ends). Runs in a thread so the caller can bound it with a timeout.
fn marker_seen<R: Read + Send + 'static>(mut r: R) -> bool {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut acc = Vec::new();
        while let Some(payload) = read_frame(&mut r) {
            acc.extend_from_slice(&payload);
            if acc.windows(MARKER.len()).any(|w| w == MARKER) {
                let _ = tx.send(true);
                return;
            }
        }
        let _ = tx.send(false);
    });
    // Generous so the marker has time to arrive even when the suite saturates
    // the CPU in parallel (same cold-load reasoning as connect_retry — only
    // costs wall-time on a genuine no-marker failure).
    rx.recv_timeout(Duration::from_secs(30)).unwrap_or(false)
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_host(sock: &std::path::Path) -> KillOnDrop {
    let child = Command::new(common::pillbox_bin())
        .args([
            "pty-host",
            "--sock",
            sock.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            AGENT_CMD,
        ])
        .spawn()
        .expect("spawn pillbox pty-host");
    KillOnDrop(child)
}

fn connect_retry(sock: &std::path::Path) -> UnixStream {
    // ~30s budget (returns as soon as connect succeeds — normally <1s). The
    // generous ceiling is for cold-start under load: this spawns the real
    // `pillbox` binary as a subprocess, and when the full suite runs in parallel
    // (the bin's unit tests + every integration binary, so threads can exceed
    // cores) the process spawn + socket bind can take many seconds under CPU
    // contention. 4s then 15s both flaked under oversubscription; a timeout only
    // costs time on a genuine hang, so the ceiling is generous. See git log.
    for _ in 0..1500 {
        if let Ok(s) = UnixStream::connect(sock) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("pty-host never started listening on {}", sock.display());
}

// Heavyweight + timing-sensitive: spawns the real `pillbox` binary (+ bash + the
// agent) as subprocesses and waits on wall-clock windows. Reliable on an idle
// machine, but flakes when the full `cargo test` suite saturates the CPU (the
// subprocesses get starved past even the generous timeouts above). So it's
// `#[ignore]`d and run deliberately:
// `cargo test --test attach -- --ignored`. It does NOT gate routine `cargo test`.
#[test]
#[ignore = "heavyweight + timing-sensitive; run with `cargo test --test attach -- --ignored`"]
fn direct_socket_attach_carries_agent_output() {
    let sock = unique_sock();
    let _host = spawn_host(&sock);
    let stream = connect_retry(&sock);
    let found = marker_seen(stream);
    let _ = std::fs::remove_file(&sock);
    assert!(
        found,
        "direct socket attach never carried the agent's marker output"
    );
}
