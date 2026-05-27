//! Integration test for the attach transport (phase 2): drive the real
//! `pillbox pty-host` subcommand, connect over its unix socket, and assert
//! the framed snapshot/stream carries the agent's output. The frame reader
//! is hand-rolled (not the crate's `Frame`) so this also pins the wire
//! format from the outside: `[type:u8][len:u32 BE][payload]`.

mod common;

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const MARKER: &[u8] = b"PILLBOXPHASE2OK";

/// A unique socket path under the system temp dir (no tempfile dep in the
/// integration-test crate).
fn unique_sock() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("pb-attach-{}-{}.sock", std::process::id(), nanos))
}

fn connect_retry(sock: &std::path::Path) -> UnixStream {
    for _ in 0..200 {
        if let Ok(s) = UnixStream::connect(sock) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("pty-host never started listening on {}", sock.display());
}

/// Read one framed payload: 5-byte header + body. `None` on EOF/timeout.
fn read_frame(s: &mut UnixStream) -> Option<Vec<u8>> {
    let mut hdr = [0u8; 5];
    s.read_exact(&mut hdr).ok()?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let mut payload = vec![0u8; len];
    s.read_exact(&mut payload).ok()?;
    Some(payload)
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn pty_host_serves_a_snapshot_carrying_agent_output() {
    let sock = unique_sock();

    // The agent: print a marker (newline so the PTY line-buffer flushes),
    // then idle so the host stays up while we attach.
    let child = Command::new(common::pillbox_bin())
        .args([
            "pty-host",
            "--sock",
            sock.to_str().unwrap(),
            "--",
            "bash",
            "-c",
            "printf 'PILLBOXPHASE2OK\\n'; sleep 10",
        ])
        .spawn()
        .expect("spawn pillbox pty-host");
    let _guard = KillOnDrop(child); // killed even if an assertion panics

    let mut stream = connect_retry(&sock);
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();

    // The marker is in the snapshot if printed before we connected, else in
    // an early Data frame — accumulate both until we see it (or time out).
    let mut acc = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while Instant::now() < deadline {
        match read_frame(&mut stream) {
            Some(payload) => {
                acc.extend_from_slice(&payload);
                if acc.windows(MARKER.len()).any(|w| w == MARKER) {
                    found = true;
                    break;
                }
            }
            None => break,
        }
    }

    let _ = std::fs::remove_file(&sock);
    assert!(
        found,
        "framed snapshot/stream never carried the agent's marker output"
    );
}
