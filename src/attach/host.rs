//! The pty-host: owns the agent's real PTY + a [`ScreenModel`], and serves
//! the [`Frame`] protocol to N clients over a unix socket. It keeps running
//! across client disconnects, so detach / `--detach` / reattach all work.
//!
//! This is the process that, in the remote backends, runs *inside* the
//! sandbox (it is `pillbox` in `pty-host` mode). A per-attach transport
//! (docker exec / ssh / e2b) carries one client's frames between the local
//! pump and one socket connection here. Locally we connect the pump to the
//! socket directly.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use super::frame::Frame;
use super::screen::ScreenModel;

const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// What the broadcast carries to each client's writer thread.
enum Out {
    Data(Vec<u8>),
    Exit(i32),
}

struct Hub {
    screen: ScreenModel,
    /// Per-client senders; pruned when a send fails.
    clients: Vec<Sender<Out>>,
}

type SharedWriter = Arc<Mutex<Box<dyn std::io::Write + Send>>>;
type SharedMaster = Arc<Mutex<Box<dyn MasterPty + Send>>>;

/// Run the pty-host: spawn `argv` under a PTY and serve frames on `sock`
/// until the child exits. `argv[0]` is the program; the rest are args.
pub(crate) fn run(sock: &str, argv: &[String]) -> Result<()> {
    let (program, args) = argv
        .split_first()
        .context("pty-host requires a command after `--`")?;

    let _ = std::fs::remove_file(sock);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening PTY")?;

    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    // portable-pty switches to explicit-env mode the moment we set any var,
    // so inherit the full environment first (the agent needs HOME, PATH,
    // and the secrets/env the backend injected via `docker run -e`), then
    // ensure a sane TERM.
    for (k, v) in std::env::vars() {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .context("spawning agent under PTY")?;

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("cloning PTY reader")?;
    let writer: SharedWriter =
        Arc::new(Mutex::new(pair.master.take_writer().context("PTY writer")?));
    drop(pair.slave); // so the master sees EOF when the child exits
    let master: SharedMaster = Arc::new(Mutex::new(pair.master));
    let hub = Arc::new(Mutex::new(Hub {
        screen: ScreenModel::new(DEFAULT_COLS, DEFAULT_ROWS),
        clients: Vec::new(),
    }));

    // PTY reader: feed the screen model + fan out raw bytes to clients.
    // Owns the child so it can read the exit code on EOF.
    {
        let hub = hub.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        let mut h = hub.lock().unwrap();
                        h.screen.feed(&chunk);
                        h.clients
                            .retain(|c| c.send(Out::Data(chunk.clone())).is_ok());
                    }
                }
            }
            // Child exited. Broadcast its code as an Exit frame so attached
            // clients can propagate it (e.g. a non-detached `pillbox run`
            // returns the agent's status), then exit so the container stops.
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(0);
            {
                let mut h = hub.lock().unwrap();
                h.clients.retain(|c| c.send(Out::Exit(code)).is_ok());
            }
            // Give the per-client writer threads a moment to encode the Exit
            // frame (each shuts its own socket right after) before the host
            // process tears down.
            thread::sleep(std::time::Duration::from_millis(250));
            std::process::exit(0);
        });
    }

    let listener = UnixListener::bind(sock).with_context(|| format!("binding {sock}"))?;
    for stream in listener.incoming().flatten() {
        let (hub, writer, master) = (hub.clone(), writer.clone(), master.clone());
        thread::spawn(move || handle_client(stream, hub, writer, master));
    }
    Ok(())
}

fn handle_client(
    stream: UnixStream,
    hub: Arc<Mutex<Hub>>,
    writer: SharedWriter,
    master: SharedMaster,
) {
    let mut wstream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };

    // Register + snapshot atomically so no live byte is missed or doubled:
    // the snapshot reflects exactly the state up to this client's first
    // queued chunk.
    let (tx, rx) = channel::<Out>();
    let snapshot = {
        let mut h = hub.lock().unwrap();
        let snap = h.screen.snapshot();
        h.clients.push(tx);
        snap
    };
    if Frame::Snapshot(snapshot).encode(&mut wstream).is_err() {
        return;
    }

    // Writer thread: broadcast -> Data/Exit frames. On Exit, flush then
    // shut the socket down so the client sees the exit code followed by a
    // clean EOF immediately — independent of how fast the host process
    // tears down afterwards (otherwise the Exit frame can race the
    // reader thread's process::exit and be lost).
    thread::spawn(move || {
        while let Ok(out) = rx.recv() {
            let (frame, is_exit) = match out {
                Out::Data(chunk) => (Frame::Data(chunk), false),
                Out::Exit(code) => (Frame::Exit(code), true),
            };
            if frame.encode(&mut wstream).is_err() {
                break;
            }
            if is_exit {
                let _ = wstream.shutdown(std::net::Shutdown::Both);
                break;
            }
        }
    });

    // This thread: client -> Input / Resize / Hello.
    let mut rstream = stream;
    loop {
        match Frame::decode(&mut rstream) {
            Ok(Some(Frame::Input(bytes))) => {
                let mut w = writer.lock().unwrap();
                if w.write_all(&bytes).and_then(|_| w.flush()).is_err() {
                    break;
                }
            }
            Ok(Some(Frame::Resize { cols, rows })) | Ok(Some(Frame::Hello { cols, rows })) => {
                let _ = master.lock().unwrap().resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                hub.lock().unwrap().screen.resize(cols, rows);
            }
            // Signal/DataAck/Unknown: no-ops in this increment (flow
            // control + signal forwarding land in phase 5 / later).
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}
