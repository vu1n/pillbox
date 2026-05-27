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

struct Hub {
    screen: ScreenModel,
    /// Per-client senders of raw PTY chunks; pruned when a send fails.
    clients: Vec<Sender<Vec<u8>>>,
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
    cmd.env("TERM", "xterm-256color");
    let _child = pair
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
                        h.clients.retain(|c| c.send(chunk.clone()).is_ok());
                    }
                }
            }
            // Child exited: the host's job is done. Real backends key
            // teardown off this; locally we just exit.
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
    let (tx, rx) = channel::<Vec<u8>>();
    let snapshot = {
        let mut h = hub.lock().unwrap();
        let snap = h.screen.snapshot();
        h.clients.push(tx);
        snap
    };
    if Frame::Snapshot(snapshot).encode(&mut wstream).is_err() {
        return;
    }

    // Writer thread: broadcast chunks -> Data frames.
    thread::spawn(move || {
        while let Ok(chunk) = rx.recv() {
            if Frame::Data(chunk).encode(&mut wstream).is_err() {
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
