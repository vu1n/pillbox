//! Remote-E2B sandbox backend — `pillbox run --remote NAME` where the
//! remote is `e2b://TEMPLATE_ID`.
//!
//! ## Flow (local side)
//!
//! 1. Resolve `--with` / `--env` exactly like the SSH backend does, into
//!    a [`VaultStdinBlob`] (the wire shape is shared — same struct, same
//!    `BLOB_VERSION`).
//! 2. Stage the blob to a local 0600 temp file. We can't pipe it through
//!    the helper subprocess's stdin because that channel forwards user
//!    keystrokes to the sandbox PTY.
//! 3. Extract the embedded Node helper script (`e2b-helper.mjs`) to
//!    `~/.pillbox/cache/e2b-helper-vX.mjs` if not already there, and
//!    exec `node helper.mjs attach --template T --blob-file F`. The
//!    helper inherits stdin/stdout/stderr.
//! 4. Helper uploads the blob into the sandbox via the E2B Files API,
//!    opens a PTY, and runs `pillbox run --vault-stdin < /tmp/blob` —
//!    the remote half is the same path the SSH backend uses.
//!
//! ## Why Node (not native Rust)
//!
//! No official E2B Rust SDK; the only third-party crate
//! (`e2b-sdk` 0.1.1) is code-interpreter only — no PTY, no
//! `commands.run`. Porting the SDK protocol natively is ~1.5K LOC of
//! HTTP + WebSocket plumbing we'd have to keep in sync with E2B's
//! release cadence. A small embedded helper is pragmatic; the cost is
//! a `node` + `npm i -g e2b` prereq on the user's machine.
//!
//! ## Why a file (not stdin) for the blob
//!
//! The PTY echoes its input by default, and turning echo off only takes
//! effect after the shell runs `stty`. If we sent the blob through the
//! PTY before then, the user's terminal would briefly display tens of
//! kilobytes of JSON (including secret material). Staging to a sandbox
//! `/tmp` file via the Files API keeps secrets off the user-visible
//! display path. The file is unlinked by the launch line as soon as
//! `pillbox run --vault-stdin` has read it.

use std::{
    collections::BTreeMap,
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
};

use anyhow::{Context, Result};

use super::remote_ssh::{InlineSecret, VaultStdinBlob, BLOB_VERSION};
use super::SandboxBackend;
use crate::agents::{workspace_mount_name, AgentSpec, RunOpts};
use crate::config::BackendKind;
use crate::errors::PillboxError;
use crate::paths::{ensure_mode_0700, pillbox_root};
use crate::pillbox::Pillbox;
use crate::remote::{E2bRef, Remote, RemoteUrl};

/// Embedded helper script — bundled into the binary so users don't have
/// to manage two files in lockstep. The cache path embeds
/// `CARGO_PKG_VERSION`, so an upgraded pillbox writes a new
/// `e2b-helper-v<new>.mjs` next to the old one and uses the new file
/// going forward; older versioned files sit on disk until the user
/// cleans `~/.pillbox/cache/`. We do NOT mutate an existing file in
/// place — that would race with a concurrent older `pillbox run`
/// reading the same path.
const HELPER_SCRIPT: &str = include_str!("e2b-helper.mjs");

/// Wire-protocol version the Rust side expects for the helper's
/// `{type:"sandbox-up"}` handshake on stderr. Bumped only when the
/// shape changes in a breaking way; the helper carries the same constant
/// (`PROTO_VERSION` in `e2b-helper.mjs`) and a mismatch fails the run
/// loudly so a stale cached helper from an older pillbox can't silently
/// drop into the new dispatcher.
const HELPER_PROTO_VERSION: u32 = 1;

/// `pillbox run --remote NAME` backend for an E2B remote.
pub(crate) struct RemoteE2bSandbox {
    remote: Remote,
}

impl RemoteE2bSandbox {
    pub(crate) fn new(remote: Remote) -> Self {
        Self { remote }
    }
}

impl SandboxBackend for RemoteE2bSandbox {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // Same workspace gating as the SSH backend: a local-rustic
        // workspace has no story for "the E2B sandbox pulls from where".
        // S3-shaped backends share config between local and sandbox, so
        // no workspace bytes have to cross the helper subprocess.
        let meta = resolved.meta.as_ref().ok_or_else(|| {
            PillboxError::usage(
                "run --remote (e2b)",
                "the global pillbox can't run remotely; cd into a project pillbox first",
            )
        })?;
        if meta.workspace.backend_kind() != BackendKind::S3 {
            return Err(PillboxError::usage(
                "run --remote (e2b)",
                "remote runs require an S3-shaped workspace backend in v0.6 \
                 (the E2B sandbox needs the same bucket/endpoint to restore from). \
                 Local-rustic via tarball transport is the PR 4.1 follow-up.",
            )
            .with_next("pillbox new --workspace-backend s3 …")
            .into());
        }

        // Same belt-and-suspenders re-validation as the SSH backend:
        // someone could hand-edit the TOML between `add` and `run`. Route
        // through `Remote::parsed_url` so the `"e2b://"` literal stays in
        // one place (`remote::parse_remote_url`).
        let e2b = match self.remote.parsed_url().map_err(|e| {
            PillboxError::config(
                "run --remote (e2b)",
                format!("remote `{}`: {e}", self.remote.name),
            )
        })? {
            RemoteUrl::E2b(e) => e,
            RemoteUrl::Ssh(_) => {
                return Err(PillboxError::config(
                    "run --remote (e2b)",
                    format!("remote `{}` is not an e2b:// URL", self.remote.name),
                )
                .into());
            }
        };

        let blob = build_blob(spec, &opts, resolved)?;
        run_via_helper(&self.remote.name, &e2b, &blob)
    }
}

/// Shared blob-build path. Mirrors the SSH backend's resolution: real
/// secret values come from THIS host's vault; only resolved values
/// cross the helper subprocess. Vaulted secrets keep their `vault_meta`
/// so the in-sandbox pillbox can re-lease them through its own session.
fn build_blob(spec: &AgentSpec, opts: &RunOpts, resolved: &Pillbox) -> Result<VaultStdinBlob> {
    let withs = crate::agents::resolve_with_entries(resolved, &opts.withs)?;
    if opts.vault && !spec.vault_capable {
        return Err(PillboxError::usage(
            "run --remote (e2b)",
            format!("--vault is not supported for `{}`", spec.id),
        )
        .into());
    }

    let mut secrets = Vec::with_capacity(withs.len());
    for w in &withs {
        let real = crate::secrets::read(resolved, &w.secret_name)?.ok_or_else(|| {
            PillboxError::runtime(
                "run --remote (e2b)",
                format!("secret `{}` not found", w.secret_name),
            )
            .with_next(format!("pillbox secret add {}", w.secret_name))
        })?;
        secrets.push(InlineSecret {
            name: w.secret_name.clone(),
            env_var: w.env_var.clone(),
            value: real.trim_end().to_string(),
            vault_meta: w.meta.clone(),
        });
    }

    let mut env = BTreeMap::new();
    for bundle in &opts.env_bundles {
        let vars = crate::envs::read(resolved, bundle)?.ok_or_else(|| {
            PillboxError::runtime(
                "run --remote (e2b)",
                format!("env bundle `{bundle}` not found"),
            )
        })?;
        for (k, v) in vars {
            env.insert(k, v);
        }
    }
    for path in &opts.env_files {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            PillboxError::runtime(
                "run --remote (e2b)",
                format!("could not read --env-file {}: {e}", path.display()),
            )
        })?;
        let vars = crate::envs::parse_dotenv(&raw, &path.display().to_string())?;
        for (k, v) in vars {
            env.insert(k, v);
        }
    }

    let workspace_host = match &opts.workspace {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("resolve current working directory")?,
    };
    let workspace_name = workspace_mount_name(&workspace_host, opts.name.as_deref())?;

    Ok(VaultStdinBlob {
        version: BLOB_VERSION,
        agent_id: spec.id.to_string(),
        agent_args: opts.args.clone(),
        workspace_mount_name: workspace_name,
        vault: opts.vault,
        secrets,
        env,
    })
}

/// Stage the blob, extract the helper, run `node helper.mjs attach …`.
fn run_via_helper(remote_name: &str, e2b: &E2bRef, blob: &VaultStdinBlob) -> Result<()> {
    let blob_bytes = blob.to_bytes()?;
    // `tempfile()` creates the file atomically via `O_CREAT | O_EXCL`
    // with mode 0o600 on Unix (see `tempfile::Builder` docs / source).
    // We write through its open handle so the staged blob never exists
    // on disk with a wider mode and we never re-open the path (which
    // would risk a symlink/race against the predictable suffix). Other
    // local users can't read the file even momentarily.
    let mut tmp = tempfile::Builder::new()
        .prefix("pillbox-e2b-blob-")
        .suffix(".json")
        .tempfile()
        .map_err(|e| PillboxError::runtime("run --remote (e2b)", format!("stage blob: {e}")))?;
    tmp.as_file_mut().write_all(&blob_bytes).map_err(|e| {
        PillboxError::runtime("run --remote (e2b)", format!("write staged blob: {e}"))
    })?;
    tmp.as_file_mut().sync_all().map_err(|e| {
        PillboxError::runtime("run --remote (e2b)", format!("flush staged blob: {e}"))
    })?;

    let helper = ensure_helper_extracted()?;
    // Match the SSH backend's wording — `(<url>)` — so the message looks
    // the same regardless of which scheme the user registered. The URL
    // string is what the user typed at `remote add`, so they recognize
    // it.
    eprintln!(
        "pillbox: connecting to `{remote_name}` (e2b://{}) …",
        e2b.template
    );

    let mut cmd = Command::new("node");
    cmd.arg(&helper)
        .arg("attach")
        .arg("--template")
        .arg(&e2b.template)
        .arg("--name")
        .arg(remote_name)
        .arg("--blob-file")
        .arg(tmp.path());
    // The user's terminal: keystrokes in, agent output out. The helper
    // writes a one-line `{type:"sandbox-up", sandboxId:"…"}` JSON to
    // stderr before any other output, so we can't blindly inherit
    // stderr without parsing — but that one line is a small price for
    // letting `node`'s own crashes flow through to the user.
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        PillboxError::resource("run --remote (e2b)", format!("could not spawn node: {e}"))
            .with_next("install Node.js + run `npm i -g e2b` (https://e2b.dev/docs)")
    })?;

    // Pump helper stderr → ours. The helper writes one JSON handshake
    // line first; we parse + verify it (proto version, sandbox id) and
    // surface the sandbox id to the user. Everything after that line is
    // a real diagnostic and passes straight through.
    let stderr = child.stderr.take().ok_or_else(|| {
        PillboxError::runtime("run --remote (e2b)", "helper stderr unexpectedly closed")
    })?;
    let stderr_thread = std::thread::spawn(move || pump_helper_stderr(stderr));

    let status = child
        .wait()
        .map_err(|e| PillboxError::runtime("run --remote (e2b)", format!("wait on helper: {e}")))?;
    // Tempfile drops here — staged blob is unlinked even if the helper
    // panicked before reading it.
    drop(tmp);
    // Receive whether the helper made it past the handshake. If not,
    // the failure is almost certainly a missing dep / unset API key, and
    // the user's terminal already has the helper's diagnostic on it —
    // we just append a generic Next: pointing at the prereqs.
    let pumped = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        let mut err = PillboxError::runtime(
            "run --remote (e2b)",
            format!("helper exited with status {status}"),
        );
        if !pumped.saw_handshake {
            err = err.with_next(
                "check the helper diagnostic above; common causes: \
                 `npm i -g e2b`, set $E2B_API_KEY, valid template id",
            );
        }
        return Err(err.into());
    }
    Ok(())
}

/// What `pump_helper_stderr` learned from the helper's stderr stream.
/// Currently just "did we see a well-formed `sandbox-up`"; the caller
/// uses it to decide whether to attach a generic prereq hint to a
/// helper-exit failure.
#[derive(Debug, Default)]
struct PumpOutcome {
    saw_handshake: bool,
}

/// Extract `e2b-helper.mjs` to `~/.pillbox/cache/e2b-helper-v<pkg>.mjs`.
/// The pkg-version suffix means an upgraded pillbox replaces the cache
/// automatically; a stale copy from an older binary can't accidentally
/// be invoked by a newer one. We check `exists()` first because the
/// happy path (every run after the first) reads only — no write needed.
pub(crate) fn ensure_helper_extracted() -> Result<PathBuf> {
    let dir = pillbox_root()?.join("cache");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // `create_dir_all` honors the process umask (typically 022 → 0755).
    // The parent `~/.pillbox/` is already 0700 so this is defense-in-
    // depth, but every other pillbox-owned dir is 0700 — keep this
    // invariant uniform so a future `chmod` audit doesn't surface a
    // stray 0755.
    ensure_mode_0700(&dir)?;
    let path = dir.join(format!("e2b-helper-v{}.mjs", env!("CARGO_PKG_VERSION")));
    if !path.exists() {
        std::fs::write(&path, HELPER_SCRIPT.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(path)
}

/// Read helper stderr line-by-line.
///
/// The first line is the helper's `{"type":"sandbox-up", protoVersion,
/// sandboxId}` JSON handshake. We parse + verify it, then surface the
/// sandbox id so the user can see what they're attached to (matches the
/// SSH backend's "connecting to NAME (URL) …" diagnostic, just delayed
/// until the sandbox is actually up).
///
/// Failure modes — all kept visible to the user, not swallowed:
///   - line isn't JSON: helper crashed before the handshake; forward it.
///   - JSON parses but `type != "sandbox-up"`: helper protocol drift;
///     forward + tag.
///   - `protoVersion` doesn't match [`HELPER_PROTO_VERSION`]: stale
///     extracted helper from an older binary; tell the user how to fix.
///
/// Everything after the handshake line is a real runtime diagnostic and
/// streams through verbatim (network errors, `sandbox.kill` failures,
/// stack traces from the SDK).
fn pump_helper_stderr(stderr: std::process::ChildStderr) -> PumpOutcome {
    let mut outcome = PumpOutcome::default();
    let mut reader = BufReader::new(stderr);
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() || first.is_empty() {
        return outcome;
    }
    let trimmed = first.trim();
    if trimmed.starts_with('{') {
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(v) => {
                let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
                let proto = v.get("protoVersion").and_then(|x| x.as_u64()).unwrap_or(0);
                let sandbox_id = v.get("sandboxId").and_then(|x| x.as_str()).unwrap_or("");
                if ty == "sandbox-up" && proto == u64::from(HELPER_PROTO_VERSION) {
                    let _ = writeln!(std::io::stderr(), "pillbox: ✓ sandbox `{sandbox_id}` up");
                    outcome.saw_handshake = true;
                } else if ty == "sandbox-up" {
                    let _ = writeln!(
                        std::io::stderr(),
                        "pillbox: helper protoVersion mismatch (got {proto}, expected {HELPER_PROTO_VERSION}). \
                         Stale extracted helper — `rm ~/.pillbox/cache/e2b-helper-*` and retry."
                    );
                } else {
                    let _ = writeln!(
                        std::io::stderr(),
                        "pillbox: unexpected helper handshake `{ty}` — forwarding raw"
                    );
                    let _ = std::io::stderr().write_all(first.as_bytes());
                }
            }
            Err(_) => {
                // JSON-looking but malformed — likely a diagnostic that
                // happens to start with `{`. Forward.
                let _ = std::io::stderr().write_all(first.as_bytes());
            }
        }
    } else {
        // The helper failed before sending the handshake. The line is the
        // actual error; pass it through.
        let _ = std::io::stderr().write_all(first.as_bytes());
    }
    // Stream everything after the handshake straight through. `io::copy`
    // pulls from the BufReader (its internal buffer keeps the post-line
    // bytes already consumed by `read_line`) and pushes to our stderr.
    // The reader keeps borrow of stderr only for the duration of the call,
    // so no intermediate buffer is needed.
    let mut sink = io::stderr();
    let _ = io::copy(&mut reader, &mut sink);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::with_isolated_home;

    #[test]
    fn helper_script_is_embedded_and_nonempty() {
        assert!(HELPER_SCRIPT.contains("Sandbox.create"));
        assert!(HELPER_SCRIPT.contains("PILLBOX_BOOT"));
    }

    #[test]
    fn helper_extracts_into_pillbox_cache() {
        with_isolated_home("e2b-helper-extract", || {
            let p = ensure_helper_extracted().unwrap();
            assert!(p.exists());
            let body = std::fs::read_to_string(&p).unwrap();
            assert!(body.starts_with("#!/usr/bin/env node"));
            // Idempotent: second call should not error / re-write.
            let p2 = ensure_helper_extracted().unwrap();
            assert_eq!(p, p2);
        });
    }

    #[test]
    fn helper_path_is_versioned() {
        with_isolated_home("e2b-helper-version", || {
            let p = ensure_helper_extracted().unwrap();
            let stem = p.file_stem().unwrap().to_str().unwrap();
            assert!(stem.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
        });
    }
}
