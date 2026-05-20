//! Remote-SSH sandbox backend — `pillbox run --remote NAME`.
//!
//! Pillbox doesn't deploy itself; the user has already installed pillbox
//! on the VPS (`brew install pillbox` / `cargo install pillbox`). PR 4
//! just teaches the local pillbox how to (a) ship one run-worth of state
//! over the wire and (b) re-spawn the agent on the remote inside its own
//! LocalDocker sandbox.
//!
//! ## Flow (local side)
//!
//! 1. Parse `--remote NAME`, look up the [`Remote`] from the resolved
//!    pillbox's `remotes/` registry (with global fallback).
//! 2. Resolve `--with` entries locally (same code path as a local run):
//!    real secret values + vault metadata.
//! 3. Refuse if the pillbox's workspace backend is `local` — PR 4 only
//!    supports S3-shaped backends (the remote uses the same config to
//!    pull from the same bucket; no data crosses SSH). Local-rustic
//!    via tarball is a PR 4.1 follow-up.
//! 4. Build a [`VaultStdinBlob`] (JSON; schema doc-commented below) and
//!    feed it via stdin to `pillbox run --vault-stdin` on the remote.
//! 5. Inherit stdout / stderr from the SSH session so the local terminal
//!    sees the agent's PTY output (the remote turns interactive too —
//!    openssh's `-tt` allocates a remote TTY).
//!
//! ## Flow (remote side)
//!
//! `pillbox run --vault-stdin` reads the blob, decodes it into a
//! [`VaultStdinBlob`], then dispatches a normal LocalDocker run with
//! pre-resolved [`InlineSecret`] entries injected directly into the env
//! (and into the vault session for vaulted secrets) — bypassing the
//! local `secrets` store on the remote. The blob is the only place
//! real secret material lives on the remote: it's not written to disk.
//!
//! ## Vault-stdin blob schema (internal — `pillbox run --vault-stdin`)
//!
//! ```jsonc
//! {
//!   "version": 1,
//!   "agent_id": "claude",
//!   "agent_args": ["--continue"],          // forwarded to the agent CLI
//!   "workspace_mount_name": "my-app",      // /workspace/<name> on the remote
//!   "vault": true,                          // route through pillbox vault proxy?
//!   "secrets": [
//!     { "name": "ANTHROPIC_API_KEY",
//!       "env_var": "ANTHROPIC_API_KEY",
//!       "value": "<real plaintext>",
//!       "vault_meta": null }                // null = plain; object = stub-swap
//!   ],
//!   "env": { "KEY": "value", ... }          // pre-merged --env / --env-file
//! }
//! ```
//!
//! Forward-compat: unknown JSON keys are ignored by serde so a newer
//! local pillbox can add fields without breaking older remotes. The
//! `version` integer, by contrast, IS enforced — a mismatch fails the
//! parse loudly so a semantic-breaking schema change can't silently
//! drop required state. Bump `BLOB_VERSION` only for breaking changes.
//! The blob never touches disk on either side.

use std::{
    fmt,
    io::{Read, Write},
    process::{Command, Stdio},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::SandboxBackend;
use crate::agents::{
    base_docker_args, workspace_mount_name, AgentSpec, RunOpts, GUEST_HOME, GUEST_WORKSPACE,
};
use crate::config::BackendKind;
use crate::docker;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::remote::{parse_ssh_url, Remote};
use crate::vault::{OAuthAgent, VaultMeta, VaultSession};

/// Wire format for `pillbox run --vault-stdin`. See module docs.
///
/// `Debug` is implemented by hand to keep secret-bearing `InlineSecret`
/// values out of any accidental `{:?}` / `dbg!` / tracing output.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct VaultStdinBlob {
    pub(crate) version: u32,
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) agent_args: Vec<String>,
    pub(crate) workspace_mount_name: String,
    #[serde(default)]
    pub(crate) vault: bool,
    #[serde(default)]
    pub(crate) secrets: Vec<InlineSecret>,
    #[serde(default)]
    pub(crate) env: std::collections::BTreeMap<String, String>,
}

impl fmt::Debug for VaultStdinBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaultStdinBlob")
            .field("version", &self.version)
            .field("agent_id", &self.agent_id)
            .field("agent_args", &self.agent_args)
            .field("workspace_mount_name", &self.workspace_mount_name)
            .field("vault", &self.vault)
            .field("secrets", &self.secrets)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// One `--with` entry's resolved form on the wire. `vault_meta` is
/// `None` for plain secrets; `Some` for vaulted ones (the remote
/// re-leases through its own vault session at swap time).
///
/// `Debug` is implemented by hand so `value` (real secret material)
/// never lands in logs / panic backtraces / tracing spans.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InlineSecret {
    pub(crate) name: String,
    pub(crate) env_var: String,
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) vault_meta: Option<VaultMeta>,
}

impl fmt::Debug for InlineSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineSecret")
            .field("name", &self.name)
            .field("env_var", &self.env_var)
            .field("value", &"<redacted>")
            .field("vault_meta", &self.vault_meta)
            .finish()
    }
}

/// Current blob version. Bumped only on breaking changes; the remote
/// rejects unknown future versions explicitly so a newer-CLI / older-
/// remote combo fails loudly instead of silently dropping required
/// fields. Unknown JSON keys WITHIN a known version are still tolerated
/// (serde default) — version bumps are reserved for semantic changes.
pub(crate) const BLOB_VERSION: u32 = 1;

impl VaultStdinBlob {
    pub(crate) fn to_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).with_context(|| "serialize vault-stdin blob".to_string())
    }

    pub(crate) fn from_bytes(buf: &[u8]) -> Result<Self> {
        // We deliberately don't surface serde_json's error detail: it
        // includes line/column/snippet of the offending bytes, which
        // could echo back partial secret material if the blob is
        // corrupted or if a user accidentally pipes a credential file
        // into `pillbox run --vault-stdin`.
        let blob: VaultStdinBlob = serde_json::from_slice(buf).map_err(|_| {
            PillboxError::config("vault-stdin", "invalid JSON blob on stdin".to_string())
        })?;
        if blob.version != BLOB_VERSION {
            return Err(PillboxError::config(
                "vault-stdin",
                format!(
                    "blob version {} not supported by this pillbox (expected {})",
                    blob.version, BLOB_VERSION
                ),
            )
            .with_next("upgrade pillbox on the remote host to match the client version")
            .into());
        }
        Ok(blob)
    }
}

/// `pillbox run --remote NAME` backend. Owns one [`Remote`] for the
/// duration of the run; the openssh `Session` lives inside `run` (it
/// holds a tempdir for the control socket, so we let `Drop` clean it
/// up at the end of the call).
pub(crate) struct RemoteSshSandbox {
    remote: Remote,
}

impl RemoteSshSandbox {
    pub(crate) fn new(remote: Remote) -> Self {
        Self { remote }
    }
}

impl SandboxBackend for RemoteSshSandbox {
    fn run(&self, spec: &AgentSpec, opts: RunOpts, resolved: &Pillbox) -> Result<()> {
        // Workspace handoff rule: S3-shaped backends only in PR 4. The
        // remote runs its own pillbox against the SAME `[workspace]`
        // config — bucket, endpoint, prefix all match, so no data has
        // to cross SSH. Local-rustic-via-tarball is the PR 4.1 follow-up.
        let meta = resolved.meta.as_ref().ok_or_else(|| {
            PillboxError::usage(
                "run --remote",
                "the global pillbox can't run remotely; cd into a project pillbox first",
            )
        })?;
        if meta.workspace.backend_kind() != BackendKind::S3 {
            return Err(PillboxError::usage(
                "run --remote",
                "remote runs require an S3-shaped workspace backend in v0.6 PR 4 \
                 (the remote rustic_core needs the same bucket/endpoint). \
                 Local-rustic via tarball transport is the PR 4.1 follow-up.",
            )
            .with_next(
                "pillbox new --workspace-backend s3 …  # or use a project that already has one",
            )
            .into());
        }

        // Resolve --with entries locally. Real secret values come from
        // THIS host's vault; only the resolved values cross SSH (once,
        // over the encrypted channel, into the remote pillbox's vault
        // session memory).
        let withs = crate::agents::resolve_with_entries(resolved, &opts.withs)?;
        if opts.vault && !spec.vault_capable {
            return Err(PillboxError::usage(
                "run --remote",
                format!("--vault is not supported for `{}`", spec.id),
            )
            .into());
        }

        let mut secrets = Vec::with_capacity(withs.len());
        for w in &withs {
            let real = crate::secrets::read(resolved, &w.secret_name)?.ok_or_else(|| {
                PillboxError::runtime(
                    "run --remote",
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

        // Pre-resolve --env / --env-file the same way LocalDocker would,
        // so the remote doesn't have to know anything about the host's
        // env bundles or local files.
        let mut env = std::collections::BTreeMap::new();
        for bundle in &opts.env_bundles {
            let vars = crate::envs::read(resolved, bundle)?.ok_or_else(|| {
                PillboxError::runtime("run --remote", format!("env bundle `{bundle}` not found"))
            })?;
            for (k, v) in vars {
                env.insert(k, v);
            }
        }
        for path in &opts.env_files {
            let raw = std::fs::read_to_string(path).map_err(|e| {
                PillboxError::runtime(
                    "run --remote",
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

        let blob = VaultStdinBlob {
            version: BLOB_VERSION,
            agent_id: spec.id.to_string(),
            agent_args: opts.args.clone(),
            workspace_mount_name: workspace_name,
            vault: opts.vault,
            secrets,
            env,
        };

        // Sanity-check the URL once more before connecting — the registry
        // validates on add, but a hand-edited file could slip through.
        let url = parse_ssh_url(&self.remote.url).map_err(|e| {
            PillboxError::config(
                "run --remote",
                format!("remote `{}`: {e}", self.remote.name),
            )
        })?;
        let destination = url.destination();

        // User-facing reassurance: SSH dial-up can stall for tens of
        // seconds on a cold connection, and the local terminal otherwise
        // sits silent until the remote sandbox prints. eprintln so we
        // don't taint stdout for `--json`-style downstream consumers.
        eprintln!(
            "pillbox: connecting to `{}` ({}) …",
            self.remote.name, self.remote.url
        );

        run_over_ssh(&destination, &blob)
    }
}

/// Connect, send the blob over stdin, proxy stdout/stderr/exit-status
/// back to the parent shell. Uses the `ssh` binary directly (single
/// short-lived subprocess). The user's `~/.ssh/config`, known_hosts,
/// agent forwarding, etc. apply transparently. When PR 6 needs
/// persistent multiplexed sessions for `session attach/detach`,
/// switching to the `openssh` crate is local to this function.
fn run_over_ssh(destination: &str, blob: &VaultStdinBlob) -> Result<()> {
    let blob_bytes = blob.to_bytes()?;

    // `-tt` forces a remote TTY even though our local stdin is a pipe.
    // Without it the agent CLI can't enter raw mode for keystroke input.
    // `-o ServerAliveInterval=30` keeps the connection from being culled
    // mid-run by aggressive NATs / firewalls.
    let mut cmd = Command::new("ssh");
    cmd.arg("-tt")
        .arg("-o")
        .arg("ServerAliveInterval=30")
        .arg(destination)
        .arg("pillbox")
        .arg("run")
        .arg("--vault-stdin");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = cmd.spawn().map_err(|e| {
        PillboxError::resource("run --remote", format!("could not spawn ssh: {e}"))
            .with_next("ensure the openssh client is on PATH (`which ssh`)")
    })?;

    {
        let stdin = child.stdin.as_mut().ok_or_else(|| {
            PillboxError::runtime("run --remote", "ssh child stdin unexpectedly closed")
        })?;
        stdin
            .write_all(&blob_bytes)
            .map_err(|e| PillboxError::runtime("run --remote", format!("send blob: {e}")))?;
    }
    // Closing stdin signals EOF to the remote so it can stop reading
    // and proceed with the agent launch. We do this by dropping the
    // handle (only way `std::process::ChildStdin` supports half-close).
    drop(child.stdin.take());

    let status = child
        .wait()
        .map_err(|e| PillboxError::runtime("run --remote", format!("wait on ssh: {e}")))?;
    if !status.success() {
        return Err(PillboxError::runtime(
            "run --remote",
            format!("ssh exited with status {status}"),
        )
        .into());
    }
    Ok(())
}

/// `pillbox run --vault-stdin` entry point — invoked by the local pillbox
/// over SSH. Reads the blob from stdin, provisions a vault session for
/// vaulted secrets, then runs the agent under the existing LocalDocker
/// sandbox path. This is the **remote** half of the protocol.
pub(crate) fn dispatch_vault_stdin(resolved: &Pillbox) -> Result<()> {
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(|e| PillboxError::runtime("run --vault-stdin", format!("read stdin: {e}")))?;
    let blob = VaultStdinBlob::from_bytes(&buf)?;

    let spec = crate::agents::ALL
        .iter()
        .copied()
        .find(|s| s.id() == blob.agent_id)
        .ok_or_else(|| {
            PillboxError::usage(
                "run --vault-stdin",
                format!("unknown agent `{}` in blob", blob.agent_id),
            )
        })?;

    docker::check_ready()?;

    let home = spec.home_dir(resolved)?;
    if !home.join(spec.cred_sentinel).exists() {
        return Err(PillboxError::runtime(
            "run --vault-stdin",
            format!("no stored credentials for `{}` on this remote", spec.id),
        )
        .with_next(format!(
            "pillbox auth login --agent {}  # run this on the REMOTE host",
            spec.id
        ))
        .into());
    }

    let workspace_host =
        std::env::current_dir().context("resolve current working directory on remote")?;
    let guest_workspace = format!("{GUEST_WORKSPACE}/{}", blob.workspace_mount_name);

    let any_vaulted = blob.secrets.iter().any(|s| s.vault_meta.is_some());
    let mut vault_session = if blob.vault || any_vaulted {
        let oauth = if blob.vault {
            Some(OAuthAgent {
                agent_id: spec.id,
                agent_home: &home,
            })
        } else {
            None
        };
        Some(VaultSession::start(oauth, resolved)?)
    } else {
        None
    };

    // Resolve --with entries using the inline real values instead of the
    // local secrets store. Vaulted entries are leased through the local
    // vault session (the stub never leaves this host).
    let mut env = blob.env.clone();
    for s in &blob.secrets {
        let injected = match (&s.vault_meta, vault_session.as_mut()) {
            (Some(meta), Some(session)) => session.lease_api_key(&s.name, &s.value, meta)?,
            (Some(_), None) => {
                return Err(PillboxError::runtime(
                    "run --vault-stdin",
                    format!(
                        "secret `{}` is marked vaulted but no vault session is active",
                        s.name
                    ),
                )
                .into());
            }
            (None, _) => s.value.clone(),
        };
        env.insert(s.env_var.clone(), injected);
    }

    let mut args = base_docker_args();
    args.extend([
        "-v".into(),
        format!("{}:{GUEST_HOME}", home.display()),
        "-v".into(),
        format!("{}:{guest_workspace}", workspace_host.display()),
        "-w".into(),
        guest_workspace,
    ]);
    for (k, v) in &env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    if let Some(session) = &vault_session {
        args.extend(session.docker_extras(GUEST_HOME));
        eprintln!(
            "pillbox: vault proxy listening on {} (ca: {})",
            session.listen_addr(),
            session.ca_cert_path().display()
        );
    }
    args.push(docker::RUNNER_IMAGE.into());
    args.extend(spec.run_argv.iter().map(|s| s.to_string()));
    args.extend(blob.agent_args.clone());

    let status = docker::run_interactive(&args)?;
    drop(vault_session);
    if !status.success() {
        return Err(PillboxError::runtime(
            "run --vault-stdin",
            format!("{} exited with status {status}", spec.id),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::HeaderScheme;

    #[test]
    fn blob_round_trips_through_json() {
        let blob = VaultStdinBlob {
            version: BLOB_VERSION,
            agent_id: "claude".into(),
            agent_args: vec!["--continue".into()],
            workspace_mount_name: "my-app".into(),
            vault: true,
            secrets: vec![
                InlineSecret {
                    name: "PLAIN".into(),
                    env_var: "PLAIN".into(),
                    value: "raw-value".into(),
                    vault_meta: None,
                },
                InlineSecret {
                    name: "ANTHROPIC_API_KEY".into(),
                    env_var: "ANTHROPIC_API_KEY".into(),
                    value: "sk-ant-api03-REAL".into(),
                    vault_meta: Some(VaultMeta::new(
                        "api.anthropic.com".into(),
                        HeaderScheme::XApiKey,
                        "sk-ant-api03-".into(),
                    )),
                },
            ],
            env: [("LOG_LEVEL".to_string(), "debug".to_string())]
                .into_iter()
                .collect(),
        };
        let bytes = blob.to_bytes().unwrap();
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back.version, BLOB_VERSION);
        assert_eq!(back.agent_id, "claude");
        assert_eq!(back.agent_args, vec!["--continue"]);
        assert_eq!(back.workspace_mount_name, "my-app");
        assert!(back.vault);
        assert_eq!(back.secrets.len(), 2);
        assert!(back.secrets[0].vault_meta.is_none());
        assert!(back.secrets[1].vault_meta.is_some());
        assert_eq!(back.env.get("LOG_LEVEL").map(String::as_str), Some("debug"));
    }

    #[test]
    fn blob_rejects_invalid_json() {
        let err = VaultStdinBlob::from_bytes(b"not json").unwrap_err();
        assert!(format!("{err}").contains("vault-stdin"));
    }

    #[test]
    fn blob_ignores_unknown_fields() {
        // Forward compatibility: a newer pillbox writing extra fields
        // shouldn't break older deserialize paths.
        let raw = serde_json::json!({
            "version": 1,
            "agent_id": "claude",
            "agent_args": [],
            "workspace_mount_name": "x",
            "vault": false,
            "secrets": [],
            "env": {},
            "future_field": {"nested": true},
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back.agent_id, "claude");
    }

    #[test]
    fn blob_defaults_for_missing_optional_fields() {
        // A minimal blob (no secrets, no env, no agent_args) should
        // deserialize cleanly so trivial agent runs don't need to write
        // empty arrays/objects.
        let raw = serde_json::json!({
            "version": 1,
            "agent_id": "codex",
            "workspace_mount_name": "p",
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let back = VaultStdinBlob::from_bytes(&bytes).unwrap();
        assert_eq!(back.agent_id, "codex");
        assert!(back.secrets.is_empty());
        assert!(back.env.is_empty());
        assert!(back.agent_args.is_empty());
        assert!(!back.vault);
    }
}
