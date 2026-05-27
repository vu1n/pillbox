//! Integration tests for the attach transport (phase 2). They drive the
//! real `pillbox` binary and assert the framed snapshot/stream carries the
//! agent's output:
//!   - direct: a client on the pty-host's unix socket.
//!   - relayed: a `pty-relay` child bridging that socket to its stdio (the
//!     in-sandbox half of the docker/ssh transport, exercised without docker).
//!
//! The frame reader is hand-rolled (not the crate's `Frame`) so these also
//! pin the wire format from the outside: `[type:u8][len:u32 BE][payload]`.

mod common;

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
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
    rx.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
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
    for _ in 0..200 {
        if let Ok(s) = UnixStream::connect(sock) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("pty-host never started listening on {}", sock.display());
}

#[test]
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

#[test]
fn pty_relay_bridges_socket_to_stdio() {
    let sock = unique_sock();
    let _host = spawn_host(&sock);
    // Wait until the host is listening, then run the relay against it.
    let _ = connect_retry(&sock);
    let mut relay = Command::new(common::pillbox_bin())
        .args(["pty-relay", "--sock", sock.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn pillbox pty-relay");
    let stdout = relay.stdout.take().expect("relay stdout");
    let _relay = KillOnDrop(relay);

    let found = marker_seen(stdout);
    let _ = std::fs::remove_file(&sock);
    assert!(
        found,
        "pty-relay never bridged the host's framed output to stdio"
    );
}

/// Full local-docker transport: launch the pty-host inside a real container,
/// attach over a `docker exec` relay, and assert the agent's output + exit
/// code propagate as frames. Ignored by default — needs docker + the runner
/// image. Run: `cargo test --test attach -- --ignored`
/// (override the image with `PILLBOX_TEST_RUNNER_IMAGE`).
#[test]
#[ignore = "requires docker + the runner image"]
fn docker_transport_propagates_output_and_exit_code() {
    const FRAME_EXIT: u8 = 8;
    let image =
        std::env::var("PILLBOX_TEST_RUNNER_IMAGE").unwrap_or_else(|_| "pillbox-runner:dev".into());
    let sock = format!("/tmp/pb-it-{}.sock", std::process::id());

    let cid = String::from_utf8(
        Command::new("docker")
            .args([
                "run",
                "-d",
                "-w",
                "/workspace",
                &image,
                "pillbox",
                "pty-host",
                "--sock",
                &sock,
                "--",
                "bash",
                "-c",
            ])
            // `pwd` proves the agent inherits the host's cwd (not $HOME).
            .arg("pwd; printf 'PILLBOXPHASE2OK\\n'; sleep 1; exit 7")
            .output()
            .expect("docker run")
            .stdout,
    )
    .expect("utf8 container id")
    .trim()
    .to_string();
    assert!(!cid.is_empty(), "docker run -d produced no container id");

    struct RmContainer(String);
    impl Drop for RmContainer {
        fn drop(&mut self) {
            let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
        }
    }
    let _rm = RmContainer(cid.clone());

    // Relay over docker exec. Keep stdin open (hold the child) so the relay
    // stays connected until the host closes after the Exit frame.
    let mut relay = Command::new("docker")
        .args(["exec", "-i", &cid, "pillbox", "pty-relay", "--sock", &sock])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("docker exec relay");
    let mut stdout = relay.stdout.take().expect("relay stdout");
    let _relay = KillOnDrop(relay);

    let mut acc = Vec::new();
    let mut exit_code = None;
    while let Some((typ, payload)) = read_typed(&mut stdout) {
        if typ == FRAME_EXIT && payload.len() == 4 {
            exit_code = Some(i32::from_be_bytes([
                payload[0], payload[1], payload[2], payload[3],
            ]));
        }
        acc.extend_from_slice(&payload);
    }

    assert!(
        acc.windows(MARKER.len()).any(|w| w == MARKER),
        "agent output did not propagate through the docker transport"
    );
    assert!(
        acc.windows(b"/workspace".len()).any(|w| w == b"/workspace"),
        "agent did not start in the workspace cwd (got $HOME instead?)"
    );
    assert_eq!(
        exit_code,
        Some(7),
        "agent exit code did not propagate as an Exit frame"
    );
}
