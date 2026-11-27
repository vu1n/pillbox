//! Which substrate runs a session or sandbox — decided once when a run starts
//! ([`SubstrateKind::selected`]) and stored once on its record ([`Substrate`]).
//!
//! A record's `backend` / `placement` / `sandbox_id` output keys are derived from
//! its [`Substrate`] here; nothing else stores or matches a backend name.

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::errors::PillboxError;
use crate::sandbox::managed::ManagedHandle;
use crate::sandbox::SandboxBackend;

/// The env var that picks the substrate a new run is placed on.
pub(crate) const BACKEND_ENV: &str = "PILLBOX_BACKEND";

/// What a new run is placed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubstrateKind {
    /// A local libkrun microVM.
    Libkrun,
    /// The managed Cloudflare execution runtime.
    Managed,
}

impl SubstrateKind {
    /// The substrate `PILLBOX_BACKEND` selects for a new run: unset (or empty) or
    /// `libkrun` → [`Libkrun`](Self::Libkrun), `managed` → [`Managed`](Self::Managed);
    /// anything else is a usage error.
    pub(crate) fn selected() -> Result<Self> {
        let value = std::env::var_os(BACKEND_ENV);
        Self::parse_selection(value.as_deref().map(|v| v.to_string_lossy()).as_deref())
    }

    fn parse_selection(value: Option<&str>) -> Result<Self> {
        const VALID: &str = "unset it (the libkrun microVM), or set it to `libkrun` or `managed`";
        match value {
            None | Some("") | Some("libkrun") => Ok(Self::Libkrun),
            Some("managed") => Ok(Self::Managed),
            Some("docker") => Err(PillboxError::usage(
                "substrate selection",
                "`PILLBOX_BACKEND=docker` was removed: pillbox no longer runs agents in Docker \
                 containers",
            )
            .with_next(VALID)
            .into()),
            Some(other) => Err(PillboxError::usage(
                "substrate selection",
                format!("`PILLBOX_BACKEND={other}` names no substrate"),
            )
            .with_next(VALID)
            .into()),
        }
    }

    /// The `backend` label a record on this substrate reports.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Libkrun => "libkrun",
            Self::Managed => "managed",
        }
    }

    /// The `placement` a record on this substrate reports.
    pub(crate) fn placement(self) -> &'static str {
        match self {
            Self::Libkrun => "local",
            Self::Managed => "managed",
        }
    }

    // The one place a substrate kind becomes a backend implementation.
    pub(crate) fn backend(self) -> Result<Box<dyn SandboxBackend>> {
        match self {
            #[cfg(feature = "libkrun")]
            Self::Libkrun => Ok(Box::new(crate::sandbox::libkrun::LibkrunBackend)),
            // The build without the libkrun feature is the in-guest tools build: it has no substrate to run on.
            #[cfg(not(feature = "libkrun"))]
            Self::Libkrun => Err(PillboxError::usage(
                "substrate",
                "this pillbox was built without a microVM substrate (the libkrun feature), so it \
                 can't start or reach a libkrun microVM",
            )
            .with_next("use a pillbox built with the libkrun feature (the default build)")
            .into()),
            Self::Managed => Ok(Box::new(crate::sandbox::managed::ManagedBackend)),
        }
    }
}

/// What a detached libkrun session or a libkrun sandbox stores on its record: the
/// persistent attach socket (libkrun-bound for the guest's listen) to reattach to,
/// the VMM child PID, and the CoW clones + spec file to scrub on teardown. The
/// detached child + these artifacts outlive the launching CLI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LibkrunHandle {
    /// Host unix socket libkrun binds for the guest's listening vsock port.
    pub(crate) sock: String,
    /// The VMM child's pid at spawn (informational; teardown reaps by `spec`).
    pub(crate) pid: i32,
    /// CoW creds/home share clone.
    pub(crate) creds: String,
    /// CoW workspace share clone.
    pub(crate) workspace: String,
    /// Persisted VmSpec path: the unique attribution key teardown reaps by.
    pub(crate) spec: String,
    /// Private rootfs clone, removed once the VMM is confirmed stopped.
    #[serde(default)]
    pub(crate) rootfs: Option<String>,
}

/// What a record runs on, with the handle that finds it again.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Substrate {
    /// A libkrun microVM. `None` = a foreground/attached session with no
    /// persistent handle.
    Libkrun(Option<LibkrunHandle>),
    /// A managed-tier session.
    Managed(ManagedHandle),
    /// A record left by a removed or unknown backend (docker, e2b, …), or with an
    /// undecodable handle: listable and removable, never driven.
    Retired {
        /// The backend label the record was written with.
        backend: String,
        /// Its raw handle (a container id, an undecodable handle's text).
        handle: String,
        /// Why pillbox can't drive it, completing "it can't be driven: …".
        why: String,
    },
}

impl Substrate {
    /// The substrate that drives this record; `None` for a retired record.
    pub(crate) fn kind(&self) -> Option<SubstrateKind> {
        match self {
            Self::Libkrun(_) => Some(SubstrateKind::Libkrun),
            Self::Managed(_) => Some(SubstrateKind::Managed),
            Self::Retired { .. } => None,
        }
    }

    /// The `backend` label output reports: the kind's, or the retired backend's.
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Retired { backend, .. } => backend,
            _ => self.kind().map_or("", SubstrateKind::label),
        }
    }

    /// The `placement` output reports; `None` for a retired record.
    pub(crate) fn placement(&self) -> Option<&'static str> {
        self.kind().map(SubstrateKind::placement)
    }

    /// The `sandbox_id` output reports: the handle's JSON, the retired record's raw
    /// handle, or "" for a foreground libkrun session.
    pub(crate) fn handle_text(&self) -> String {
        match self {
            Self::Libkrun(None) => String::new(),
            Self::Libkrun(Some(handle)) => {
                serde_json::to_string(handle).expect("a libkrun handle serializes")
            }
            Self::Managed(handle) => {
                serde_json::to_string(handle).expect("a managed handle serializes")
            }
            Self::Retired { handle, .. } => handle.clone(),
        }
    }

    /// For a retired record: what removing it leaves behind, and the manual
    /// cleanup if any.
    pub(crate) fn left_behind(&self) -> String {
        match self {
            Self::Retired {
                backend, handle, ..
            } => match backend.as_str() {
                LEGACY_DOCKER => format!(
                    "its container `{handle}` was left as it was; pillbox no longer runs docker, \
                     remove it with `docker rm -f {handle}`"
                ),
                LEGACY_LIBKRUN => "any VM or files its handle named were not torn down".into(),
                _ => "nothing it ran was torn down".into(),
            },
            _ => String::new(),
        }
    }

    /// Decode a stored substrate: a `[substrate]` table's `kind` and its other fields.
    fn decode(kind: &str, fields: serde_json::Map<String, serde_json::Value>) -> Self {
        match kind {
            "retired" => {
                let text = |key: &str| {
                    fields
                        .get(key)
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                Self::Retired {
                    backend: text("backend"),
                    handle: text("handle"),
                    why: text("why"),
                }
            }
            LEGACY_LIBKRUN if fields.is_empty() => Self::Libkrun(None),
            _ => {
                let raw = serde_json::Value::Object(fields).to_string();
                Self::from_legacy(kind, &raw)
            }
        }
    }

    /// Map a pre-substrate record's `backend` label and raw handle (`sandbox_id` /
    /// `backend_ref`).
    // A handle that won't decode retires the record instead of failing its read, so it stays listable and removable.
    pub(crate) fn from_legacy(backend: &str, handle: &str) -> Self {
        let retired = |why: String| Self::Retired {
            backend: backend.to_string(),
            handle: handle.to_string(),
            why,
        };
        match backend {
            LEGACY_LIBKRUN if handle.is_empty() => Self::Libkrun(None),
            LEGACY_LIBKRUN => match serde_json::from_str(handle) {
                Ok(handle) => Self::Libkrun(Some(handle)),
                Err(e) => retired(format!("its libkrun handle can't be decoded ({e})")),
            },
            LEGACY_MANAGED => match serde_json::from_str(handle) {
                Ok(handle) => Self::Managed(handle),
                Err(e) => retired(format!("its managed handle can't be decoded ({e})")),
            },
            LEGACY_DOCKER => retired("pillbox's Docker backend was removed".into()),
            other => retired(format!("pillbox has no `{other}` backend")),
        }
    }
}

/// Pre-substrate `backend` labels, read only to migrate old records.
const LEGACY_DOCKER: &str = "docker";
const LEGACY_LIBKRUN: &str = "libkrun";
const LEGACY_MANAGED: &str = "managed";

/// Rewrite a pre-substrate record (no `[substrate]` table) into the current shape:
/// its `backend` + `handle_key` (`sandbox_id` for a session, `backend_ref` for a
/// sandbox) become a `[substrate]` table, and the derived `placement` is dropped.
/// A record without a `backend` is left alone (its read then fails as before).
// Old records migrate on read; the next write of the record stores the new shape.
pub(crate) fn migrate_legacy(record: &mut toml::Table, handle_key: &str) {
    if record.contains_key("substrate") {
        return;
    }
    let Some(toml::Value::String(backend)) = record.remove("backend") else {
        return;
    };
    let handle = match record.remove(handle_key) {
        Some(toml::Value::String(handle)) => handle,
        _ => String::new(),
    };
    record.remove("placement");
    let substrate = Substrate::from_legacy(&backend, &handle);
    let table = toml::Value::try_from(&substrate).expect("a substrate serializes to a TOML table");
    record.insert("substrate".into(), table);
}

/// The on-disk `[substrate]` table: `kind` plus the handle's fields.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Stored<'a> {
    Libkrun {
        #[serde(flatten)]
        handle: Option<&'a LibkrunHandle>,
    },
    Managed(&'a ManagedHandle),
    Retired {
        backend: &'a str,
        handle: &'a str,
        why: &'a str,
    },
}

impl Serialize for Substrate {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Libkrun(handle) => Stored::Libkrun {
                handle: handle.as_ref(),
            },
            Self::Managed(handle) => Stored::Managed(handle),
            Self::Retired {
                backend,
                handle,
                why,
            } => Stored::Retired {
                backend,
                handle,
                why,
            },
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Substrate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            kind: String,
            #[serde(flatten)]
            fields: serde_json::Map<String, serde_json::Value>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self::decode(&raw.kind, raw.fields))
    }
}

impl std::fmt::Display for Substrate {
    /// One line for `session info`: the kind and a summary of its handle.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Libkrun(None) => write!(f, "libkrun (foreground, no persistent handle)"),
            Self::Libkrun(Some(handle)) => write!(
                f,
                "libkrun (vmm pid {}, attach socket {})",
                handle.pid, handle.sock
            ),
            Self::Managed(handle) => write!(
                f,
                "managed ({}, execution session {})",
                handle.endpoint, handle.execution_session_id
            ),
            Self::Retired {
                backend,
                handle,
                why,
            } => write!(f, "retired `{backend}` record, handle `{handle}` ({why})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LK: &str = r#"{"sock":"/tmp/x.sock","pid":1,"creds":"/tmp/c","workspace":"/tmp/w","spec":"/tmp/s.json"}"#;
    const MG: &str =
        r#"{"endpoint":"https://w.example.workers.dev","execution_session_id":"aaaaaaaaaaa3"}"#;

    fn lk() -> LibkrunHandle {
        LibkrunHandle {
            sock: "/tmp/x.sock".into(),
            pid: 1,
            creds: "/tmp/c".into(),
            workspace: "/tmp/w".into(),
            spec: "/tmp/s.json".into(),
            rootfs: None,
        }
    }

    fn mg() -> ManagedHandle {
        ManagedHandle {
            endpoint: "https://w.example.workers.dev".into(),
            execution_session_id: "aaaaaaaaaaa3".into(),
        }
    }

    /// A pre-substrate session record's TOML, migrated the way a read does it.
    fn migrated(backend: &str, sandbox_id: &str, extra: &str) -> Substrate {
        let raw = format!(
            "id = \"aaaaaaaaaaa1\"\nbackend = \"{backend}\"\nsandbox_id = '{sandbox_id}'\n{extra}"
        );
        let mut table: toml::Table = toml::from_str(&raw).unwrap();
        migrate_legacy(&mut table, "sandbox_id");
        assert!(!table.contains_key("backend") && !table.contains_key("sandbox_id"));
        assert!(!table.contains_key("placement"));
        table["substrate"].clone().try_into().unwrap()
    }

    #[test]
    fn a_legacy_record_of_each_kind_loads_to_its_substrate() {
        let docker = migrated("docker", "0123456789abcdef", "");
        assert_eq!(docker.kind(), None);
        assert_eq!(docker.label(), "docker");
        assert_eq!(docker.placement(), None);
        assert_eq!(docker.handle_text(), "0123456789abcdef");
        assert!(
            docker
                .left_behind()
                .contains("docker rm -f 0123456789abcdef"),
            "{}",
            docker.left_behind()
        );

        assert_eq!(migrated("libkrun", LK, ""), Substrate::Libkrun(Some(lk())));
        assert_eq!(migrated("libkrun", "", ""), Substrate::Libkrun(None));
        assert_eq!(
            migrated("managed", MG, "placement = \"managed\""),
            Substrate::Managed(mg())
        );

        let e2b = migrated("e2b", "sb-1", "placement = \"local\"");
        assert!(
            matches!(&e2b, Substrate::Retired { backend, handle, .. } if backend == "e2b" && handle == "sb-1"),
            "{e2b:?}"
        );
        assert_eq!(e2b.label(), "e2b");
        assert!(!e2b.left_behind().contains("docker"));
    }

    #[test]
    fn a_legacy_handle_that_does_not_decode_retires_the_record() {
        for backend in ["libkrun", "managed"] {
            let substrate = migrated(backend, "container-abc", "");
            match &substrate {
                Substrate::Retired {
                    backend: b,
                    handle,
                    why,
                } => {
                    assert_eq!((b.as_str(), handle.as_str()), (backend, "container-abc"));
                    assert!(why.contains("can't be decoded"), "{why}");
                }
                other => panic!("{backend}: expected Retired, got {other:?}"),
            }
        }
    }

    #[test]
    fn substrates_round_trip_through_their_table() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Record {
            id: String,
            substrate: Substrate,
        }
        for substrate in [
            Substrate::Libkrun(None),
            Substrate::Libkrun(Some(lk())),
            Substrate::Managed(mg()),
            Substrate::from_legacy("docker", "0123"),
        ] {
            let record = Record {
                id: "x".into(),
                substrate,
            };
            let text = toml::to_string(&record).unwrap();
            assert!(text.contains("[substrate]"), "{text}");
            let back: Record = toml::from_str(&text).unwrap();
            assert_eq!(back, record, "{text}");
        }
        let text = toml::to_string(&Record {
            id: "x".into(),
            substrate: Substrate::Libkrun(Some(lk())),
        })
        .unwrap();
        assert!(text.contains("kind = \"libkrun\""), "{text}");
        assert!(text.contains("sock = \"/tmp/x.sock\""), "{text}");
    }

    #[test]
    fn derived_keys_match_the_pre_substrate_output() {
        let libkrun = Substrate::Libkrun(Some(lk()));
        assert_eq!(
            (libkrun.label(), libkrun.placement()),
            ("libkrun", Some("local"))
        );
        // The handle's JSON, as the record wrote it before (`rootfs` included).
        assert_eq!(libkrun.handle_text(), LK.replace('}', r#","rootfs":null}"#));
        let managed = Substrate::Managed(mg());
        assert_eq!(
            (managed.label(), managed.placement()),
            ("managed", Some("managed"))
        );
        assert_eq!(managed.handle_text(), MG);
        assert_eq!(Substrate::Libkrun(None).handle_text(), "");
    }

    #[test]
    fn selection_defaults_to_libkrun_and_refuses_docker() {
        use SubstrateKind::*;
        for (value, want) in [
            (None, Libkrun),
            (Some(""), Libkrun),
            (Some("libkrun"), Libkrun),
        ] {
            assert_eq!(SubstrateKind::parse_selection(value).unwrap(), want);
        }
        assert_eq!(
            SubstrateKind::parse_selection(Some("managed")).unwrap(),
            Managed
        );
        for value in ["docker", "e2b"] {
            let err = SubstrateKind::parse_selection(Some(value)).unwrap_err();
            let pillbox = err.downcast_ref::<PillboxError>().expect("a PillboxError");
            assert!(
                matches!(pillbox.category, crate::errors::ExitCategory::Usage),
                "{value}: {err}"
            );
            let text = err.to_string();
            assert!(text.contains(value), "{text}");
            assert!(text.contains("managed"), "{text}");
        }
        let docker = SubstrateKind::parse_selection(Some("docker")).unwrap_err();
        assert!(docker.to_string().contains("removed"), "{docker}");
    }
}
