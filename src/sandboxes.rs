//! Sandbox registry — durable handles for long-lived microVMs spawned
//! via `pillbox sandbox spawn` and addressed later by `exec` / `destroy`.
//!
//! A *sandbox* is a running VM with a workspace clone mounted, kept alive
//! so a consumer (orchestrator, Slack bot, hermes) can `exec` commands into
//! it over the PTY-free contract. This is the passive addressing layer the
//! `exec`/`destroy` verbs need — `sandbox_id → substrate handle` — NOT an
//! orchestrator: it records what exists, it never decides what to run.
//!
//! Storage mirrors [`crate::session`]: one TOML file per record under
//! `<pillbox>/sandboxes/<id>.toml`, single-scope (no inheritance — a sandbox
//! is concrete runtime state tied to the pillbox that spawned it), via the
//! shared [`crate::registry::Registry`] plumbing.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::registry::{self as reg, IdRegistry, Registry};
use crate::sandbox::Substrate;

const SANDBOXES_DIR: &str = "sandboxes";

/// On-disk shape. Forward-compatible: serde ignores unknown fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Sandbox {
    /// Short hex id (12 chars) — registry filename + the `exec`/`destroy` arg.
    pub(crate) id: String,
    /// Runner image the sandbox's rootfs was materialized from.
    pub(crate) image: String,
    /// Agent harness this sandbox is provisioned for (auth mounted, non-root),
    /// or `None` for a bare exec-only sandbox.
    #[serde(default)]
    pub(crate) agent: Option<String>,
    /// Host workspace the sandbox was spawned from (libkrun clones it).
    pub(crate) workspace: String,
    /// Host path of the sandbox's CoW, secret-scrubbed workspace clone (libkrun),
    /// where its writes land; `None` for a record that kept no clone.
    #[serde(default)]
    pub(crate) workspace_clone: Option<String>,
    /// Where the guest mounts that clone (libkrun): the cwd of every exec.
    #[serde(default)]
    pub(crate) guest_workspace: Option<String>,
    /// Optional human label, surfaced in `sandbox list`.
    #[serde(default)]
    pub(crate) label: Option<String>,
    /// RFC3339 spawn timestamp.
    pub(crate) created_at: String,
    /// Lifecycle status. Always `ready` today (destroy deletes the record);
    /// kept for forward-compat and `list` display.
    pub(crate) status: String,
    /// What this sandbox runs on, with the handle that finds it again (a libkrun
    /// sandbox: its sockets, spec and clones). No scalar field may follow this
    /// table: TOML would parse it into `[substrate]`.
    pub(crate) substrate: Substrate,
}

impl Sandbox {
    /// Mint a 12-hex-char id — see [`crate::registry::new_id`].
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn new_id() -> String {
        reg::new_id()
    }

    /// Where [`write`] persists sandbox `id`: `<pillbox>/sandboxes/<id>.toml`. A
    /// reparented libkrun VMM's commit guard watches it — the VM self-destructs if
    /// this file doesn't appear before its launcher dies.
    #[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
    pub(crate) fn record_path(pb: &Pillbox, id: &str) -> std::path::PathBuf {
        SandboxRegistry::path_read(pb, id)
    }
}

/// Parse a sandbox record, migrating a pre-substrate one (`backend` +
/// `backend_ref`; see [`crate::sandbox::substrate::migrate_legacy`]).
fn decode(raw: &str) -> std::result::Result<Sandbox, toml::de::Error> {
    let mut table: toml::Table = toml::from_str(raw)?;
    crate::sandbox::substrate::migrate_legacy(&mut table, "backend_ref");
    toml::Value::Table(table).try_into()
}

struct SandboxRegistry;
impl Registry for SandboxRegistry {
    type Record = Sandbox;
    const SUBDIR: &'static str = SANDBOXES_DIR;
    fn read_action() -> &'static str {
        "sandbox read"
    }
    fn filename(name: &str) -> String {
        format!("{name}.toml")
    }
    fn parse(raw: &str, source: &Path) -> Result<Self::Record> {
        decode(raw).map_err(|e| {
            PillboxError::config("sandbox read", format!("{}: {e}", source.display())).into()
        })
    }
}
impl IdRegistry for SandboxRegistry {
    const ENTITY: &'static str = "sandbox";
    fn record_id(record: &Sandbox) -> &str {
        &record.id
    }
}

#[cfg_attr(not(feature = "libkrun"), allow(dead_code))]
pub(crate) fn write(pb: &Pillbox, sandbox: &Sandbox) -> Result<()> {
    let body = toml::to_string(sandbox)
        .map_err(|e| PillboxError::config("sandbox write", e.to_string()))?;
    reg::write_record::<SandboxRegistry>(pb, &sandbox.id, body.as_bytes())
}

pub(crate) fn delete(pb: &Pillbox, id: &str) -> Result<()> {
    SandboxRegistry::delete(pb, id).map(|_| ())
}

/// All sandboxes in the current pillbox, oldest first.
pub(crate) fn list(pb: &Pillbox) -> Result<Vec<Sandbox>> {
    let mut out = reg::list_all::<SandboxRegistry>(pb)?;
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(out)
}

/// Resolve an exact id or unique prefix (>= 4 chars).
pub(crate) fn resolve(pb: &Pillbox, id_or_prefix: &str) -> Result<Sandbox> {
    reg::resolve_id::<SandboxRegistry>(pb, id_or_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox;
    use crate::test_util::with_isolated_home;

    fn make() -> Sandbox {
        Sandbox {
            id: Sandbox::new_id(),
            substrate: Substrate::Libkrun(None),
            image: "pillbox:latest".into(),
            agent: None,
            workspace: "/work/app".into(),
            workspace_clone: None,
            guest_workspace: None,
            label: None,
            created_at: crate::session::now_rfc3339(),
            status: "ready".into(),
        }
    }

    #[test]
    fn record_path_is_where_write_persists() {
        with_isolated_home("sandbox-record-path", || {
            let g = pillbox::global();
            let s = make();
            assert!(!Sandbox::record_path(&g, &s.id).exists());
            write(&g, &s).unwrap();
            assert!(Sandbox::record_path(&g, &s.id).exists());
        });
    }

    #[test]
    fn a_legacy_sandbox_record_migrates_to_its_substrate_and_rewrites_new() {
        let lk = r#"{"sock":"/s","pid":7,"creds":"/c","workspace":"/w","spec":"/p","rootfs":null}"#;
        for (backend, handle) in [("docker", "container-abc"), ("libkrun", lk), ("e2b", "x")] {
            let raw = format!(
                "id = \"abcdef012345\"\nbackend = \"{backend}\"\nbackend_ref = '{handle}'\n\
                 image = \"i\"\nworkspace = \"/w\"\ncreated_at = \"t\"\nstatus = \"ready\"\n"
            );
            let record = decode(&raw).expect("a legacy sandbox record parses");
            assert_eq!(record.substrate, Substrate::from_legacy(backend, handle));
            assert_eq!(record.substrate.label(), backend);
            assert_eq!(record.substrate.handle_text(), handle);
            let rewritten = toml::to_string(&record).unwrap();
            assert!(!rewritten.contains("backend_ref"), "{rewritten}");
            assert_eq!(decode(&rewritten).unwrap(), record);
        }
    }

    #[test]
    fn new_id_is_12_hex_chars() {
        let id = Sandbox::new_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn write_resolve_delete_round_trip() {
        with_isolated_home("sandbox-rt", || {
            let g = pillbox::global();
            let s = make();
            write(&g, &s).unwrap();
            assert_eq!(resolve(&g, &s.id).unwrap(), s);
            delete(&g, &s.id).unwrap();
            assert!(resolve(&g, &s.id).is_err()); // gone
            delete(&g, &s.id).unwrap(); // idempotent
        });
    }

    #[test]
    fn resolve_accepts_prefix_and_rejects_short() {
        with_isolated_home("sandbox-resolve", || {
            let g = pillbox::global();
            let s = make();
            write(&g, &s).unwrap();
            assert_eq!(resolve(&g, &s.id[..6]).unwrap().id, s.id);
            assert!(resolve(&g, "abc")
                .unwrap_err()
                .to_string()
                .contains("too short"));
        });
    }

    #[test]
    fn list_is_oldest_first() {
        with_isolated_home("sandbox-list", || {
            let g = pillbox::global();
            let mut a = make();
            a.created_at = "2026-01-01T00:00:00Z".into();
            let mut b = make();
            b.created_at = "2026-02-01T00:00:00Z".into();
            write(&g, &b).unwrap();
            write(&g, &a).unwrap();
            let all = list(&g).unwrap();
            assert_eq!(all.len(), 2);
            assert_eq!(all[0].created_at, "2026-01-01T00:00:00Z");
        });
    }
}
