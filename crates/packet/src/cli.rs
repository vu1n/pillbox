//! The `pack` and `unpack` subcommands: one definition of their flags and
//! output, shared by the `pillbox-packet` binary and `pillbox checkpoint`.
//! Both are self-contained from flags: no pillbox is resolved.

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;
use serde_json::json;

use crate::{pack, unpack, PackOptions, UnpackOptions, FORMAT};

#[derive(Subcommand, Debug)]
pub enum Command {
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
        /// Leave every workspace directory with this name out, with all under
        /// it (e.g. `node_modules`); repeatable. Reported as `skipped`.
        #[arg(long = "skip-dir", value_name = "NAME")]
        skip_dirs: Vec<String>,
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

/// How the calling binary names itself on its human-readable lines.
pub struct Names {
    /// Line prefix before `: `, e.g. `pillbox`.
    pub program: &'static str,
    /// Words before the subcommand name in stderr notes, e.g. `checkpoint `
    /// (with its trailing space) or empty.
    pub command: &'static str,
}

/// Run one subcommand and print its result. The JSON is a versioned contract
/// (Stratus parses it); the human lines are not.
pub fn run(cmd: Command, names: &Names) -> Result<()> {
    let Names { program, command } = names;
    match cmd {
        Command::Pack {
            workspace,
            harness_state,
            harness,
            harness_session_id,
            transcript,
            out,
            skip_dirs,
            json,
        } => {
            let packed = pack(&PackOptions {
                workspace,
                harness_state,
                harness,
                harness_session_id,
                transcript,
                out: out.clone(),
                skip_dirs,
            })?;
            for e in &packed.excluded {
                eprintln!(
                    "{program}: {command}pack: excluded {} ({})",
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
                    "{program}: ✓ packed {} ({} bytes, {} entries) to {}",
                    packed.digest,
                    packed.bytes,
                    packed.manifest.entries.len(),
                    out.display()
                );
            }
            Ok(())
        }
        Command::Unpack {
            packet,
            expect,
            workspace,
            harness_state,
            allow_nonempty,
            json,
        } => {
            let unpacked = unpack(&UnpackOptions {
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
                    "{program}: ✓ unpacked {} ({} entries) into {}",
                    unpacked.digest,
                    unpacked.manifest.entries.len(),
                    workspace.display()
                );
            }
            Ok(())
        }
    }
}
