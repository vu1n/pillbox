//! Sandbox registry — durable handles for long-lived containers spawned
//! via `pillbox sandbox spawn` and addressed later by `exec` / `destroy`.
//!
//! A *sandbox* is a running container with a workspace mounted, kept alive
//! so a consumer (orchestrator, Slack bot, hermes) can `exec` commands into
//! it over the PTY-free contract. This is the passive addressing layer the
//! `exec`/`destroy` verbs need — `sandbox_id → backend handle` — NOT an
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

const SANDBOXES_DIR: &str = "sandboxes";

/// Backend label written into the record. String (not enum) on disk so a
/// future binary that adds a backend can still read older records.
pub(crate) const BACKEND_DOCKER: &str = "docker";

/// On-disk shape. Forward-compatible: serde ignores unknown fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Sandbox {
    /// Short hex id (12 chars) — registry filename + the `exec`/`destroy` arg.
    pub(crate) id: String,
    /// Backend kind — [`BACKEND_DOCKER`] today; `e2b` / `ssh` later.
    pub(crate) backend: String,
    /// Opaque handle the backend uses to find the container again. For
    /// docker: the container id. For e2b (future): the sandbox id.
    pub(crate) backend_ref: String,
    /// Runner image the container was started from.
    pub(crate) image: String,
    /// Host path mounted as the workspace.
    pub(crate) workspace: String,
    /// Optional human label, surfaced in `sandbox list`.
    #[serde(default)]
    pub(crate) label: Option<String>,
    /// RFC3339 spawn timestamp.
    pub(crate) created_at: String,
    /// Lifecycle status. Always `ready` today (destroy deletes the record);
    /// kept for forward-compat and `list` display.
    pub(crate) status: String,
}

impl Sandbox {
    /// Mint a 12-hex-char id — see [`crate::registry::new_id`].
    pub(crate) fn new_id() -> String {
        reg::new_id()
    }
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
        toml::from_str(raw).map_err(|e| {
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
            backend: BACKEND_DOCKER.into(),
            backend_ref: "container-abc".into(),
            image: "pillbox:latest".into(),
            workspace: "/work/app".into(),
            label: None,
            created_at: crate::session::now_rfc3339(),
            status: "ready".into(),
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
