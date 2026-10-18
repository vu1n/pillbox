//! `pillbox checkpoint pack|unpack` — the session packet engine subcommands
//! Stratus calls (see `src/checkpoint/` and `docs/session-packet.md`). Both are
//! self-contained from flags: no pillbox is resolved.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde_json::json;

use crate::checkpoint::{self, PackOptions, UnpackOptions, FORMAT};

#[derive(Subcommand, Debug)]
pub(crate) enum CheckpointAction {
    /// Write a workspace (and optionally a harness's state) to one
    /// deterministic session packet. Credential files are never packed.
    Pack {
        /// Directory packed as the packet's `workspace/` tree.
        #[arg(long, value_name = "DIR")]
        workspace: PathBuf,
        /// Directory packed as the packet's `harness/` tree (e.g. `~/.claude`).
        #[arg(long = "harness-state", value_name = "DIR")]
        harness_state: Option<PathBuf>,
        /// Harness id recorded in the manifest (e.g. `claude_code`).
        #[arg(long, value_name = "NAME")]
        harness: String,
        /// The harness's own session id, recorded in the manifest.
        #[arg(long = "harness-session-id", value_name = "ID")]
        harness_session_id: Option<String>,
        /// Transcript path relative to `--harness-state`; must be a packed file.
        #[arg(long, value_name = "REL")]
        transcript: Option<String>,
        /// Packet file to write (replaced atomically).
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
        /// Print the versioned JSON result on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Verify a session packet against its digest and manifest, then unpack it.
    /// Nothing is written unless the whole packet verifies.
    Unpack {
        /// Packet file to read.
        #[arg(long, value_name = "FILE")]
        packet: PathBuf,
        /// Expected packet digest, `sha256:<64 hex>`.
        #[arg(long, value_name = "DIGEST")]
        expect: String,
        /// Directory the `workspace/` tree unpacks into (created if missing).
        #[arg(long, value_name = "DIR")]
        workspace: PathBuf,
        /// Directory the `harness/` tree unpacks into. Required when the
        /// packet carries harness state.
        #[arg(long = "harness-state", value_name = "DIR")]
        harness_state: Option<PathBuf>,
        /// Unpack over non-empty targets: packet paths replace what is there,
        /// other files are kept. Without it a non-empty target is refused.
        #[arg(long = "allow-nonempty")]
        allow_nonempty: bool,
        /// Print the versioned JSON result on stdout.
        #[arg(long)]
        json: bool,
    },
}

pub(crate) fn dispatch(action: CheckpointAction) -> Result<()> {
    match action {
        CheckpointAction::Pack {
            workspace,
            harness_state,
            harness,
            harness_session_id,
            transcript,
            out,
            json,
        } => {
            let packed = checkpoint::pack(&PackOptions {
                workspace,
                harness_state,
                harness,
                harness_session_id,
                transcript,
                out: out.clone(),
            })?;
            for e in &packed.excluded {
                eprintln!(
                    "pillbox: checkpoint pack: excluded {} ({})",
                    e.path, e.reason
                );
            }
            if json {
                let out = json!({
                    "version": 1,
                    "format": FORMAT,
                    "digest": packed.digest,
                    "bytes": packed.bytes,
                    "manifest": packed.manifest,
                    "excluded": packed.excluded,
                });
                println!("{out}");
            } else {
                println!(
                    "pillbox: ✓ packed {} ({} bytes, {} entries) to {}",
                    packed.digest,
                    packed.bytes,
                    packed.manifest.entries.len(),
                    out.display()
                );
            }
            Ok(())
        }
        CheckpointAction::Unpack {
            packet,
            expect,
            workspace,
            harness_state,
            allow_nonempty,
            json,
        } => {
            let unpacked = checkpoint::unpack(&UnpackOptions {
                packet,
                expect,
                workspace: workspace.clone(),
                harness_state,
                allow_nonempty,
            })?;
            if json {
                let out = json!({
                    "version": 1,
                    "format": FORMAT,
                    "digest": unpacked.digest,
                    "manifest": unpacked.manifest,
                });
                println!("{out}");
            } else {
                println!(
                    "pillbox: ✓ unpacked {} ({} entries) into {}",
                    unpacked.digest,
                    unpacked.manifest.entries.len(),
                    workspace.display()
                );
            }
            Ok(())
        }
    }
}
