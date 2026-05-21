//! Per-pillbox single-file registries — shared scaffolding for
//! `secrets/`, `env/`, `remotes/`, and `sessions/`.
//!
//! Each registry is a directory under a `Pillbox`'s state dir holding
//! one file per record (filename keyed by name / id). The four
//! consumers each had their own near-identical `dir / dir_read /
//! path / path_read` helpers and — for inherited registries — an
//! identical `read_inherited` walking [`Pillbox::read_chain`]. The
//! flagging-of-this-four-times in code review prompted the lift.
//!
//! ## Trait split
//!
//! - [`Registry`] — path layout + parse/serialize for a record type.
//!   Single-scope reads/writes only. Implemented by every consumer.
//! - [`InheritedRegistry`] — marker trait that opts a registry into
//!   the project→global walk plus the [`merged`] listing. Sessions
//!   intentionally don't implement it: a session is concrete runtime
//!   state tied to the pillbox that started it, not config to inherit.
//!
//! ## Variations the trait does NOT abstract
//!
//! - **Secrets' `.meta.json` sidecar** stays in `secrets.rs` — it's a
//!   parallel registry (filename `<name>.meta.json`, same dir) handled
//!   by a separate code path. Lifting it would require either filtering
//!   inside this module or two trait impls, both worse than the
//!   localized sidecar handling.
//! - **Envs' `parse_dotenv`** stays in `envs.rs`. Bundle records on
//!   disk are the raw file content (a `String`); the KV parse happens
//!   at the caller. So `EnvBundle::parse` returns `String` and callers
//!   pass it to `parse_dotenv` when they need the map.
//! - **Remotes' URL re-validation in `parse_remote`** lives inside the
//!   record's `parse` impl — exactly where it should: a malformed URL
//!   on disk turns into a config error at read time. The trait surface
//!   doesn't need a separate hook for it.
//! - **Sessions' no-inheritance** is encoded by not implementing
//!   `InheritedRegistry`. The `read_inherited` and `list_merged`
//!   helpers simply don't exist for sessions.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::paths::{validate_name, write_private_file};
use crate::pillbox::{Pillbox, Scope};

/// One on-disk file under a pillbox-scoped registry directory.
///
/// Implementors describe:
/// - which subdirectory the records live in ([`SUBDIR`]),
/// - what each filename looks like ([`filename`]),
/// - how to parse a record from the file body and re-serialize one,
/// - the human-facing action label used in `validate_name` errors.
///
/// All the path-shuffling, scope-walking, listing, and file IO lives
/// here as default methods so consumers only write the type-specific
/// glue.
///
/// [`SUBDIR`]: Registry::SUBDIR
/// [`filename`]: Registry::filename
pub(crate) trait Registry: Sized {
    /// Owned/parsed record type, e.g. `Remote`, `Session`, `String`
    /// (for secrets/env bundles where the file body is the value).
    type Record;

    /// Subdirectory name under the pillbox state dir
    /// (e.g. `"remotes"`, `"sessions"`, `"secrets"`, `"env"`).
    const SUBDIR: &'static str;

    /// Action label used in `validate_name` and parse-error context
    /// (e.g. `"remote read"`). Kept as a method (not a const) so
    /// consumers can vary it per call site if needed; the default uses
    /// the type's natural action.
    fn read_action() -> &'static str;

    /// Translate a record name / id into the on-disk filename. Many
    /// registries add a `.toml` suffix; secrets and env bundles use
    /// the raw name.
    fn filename(name: &str) -> String;

    /// Parse the file body. Receives the source path purely for error
    /// context (e.g. `"<path>: <inner>"`). Implementors that also need
    /// to re-validate fields (URL shape, etc.) do it here so the
    /// failure surfaces at read time, not connect time.
    fn parse(raw: &str, source: &Path) -> Result<Self::Record>;

    // ── Default-provided helpers ────────────────────────────────────

    /// Write-side dir path: `mkdir -p` + 0700. Mirrors what each
    /// `*_dir` helper used to do.
    fn dir(pb: &Pillbox) -> Result<PathBuf> {
        pb.subdir(Self::SUBDIR)
    }

    /// Read-side dir path: just join, no `mkdir`. Hot-path readers
    /// (`pillbox run`-time secret/env lookups) shouldn't pay
    /// `create_dir_all` syscalls per call.
    fn dir_read(pb: &Pillbox) -> PathBuf {
        pb.subdir_path(Self::SUBDIR)
    }

    /// Write-side full record path.
    fn path(pb: &Pillbox, name: &str) -> Result<PathBuf> {
        Ok(Self::dir(pb)?.join(Self::filename(name)))
    }

    /// Read-side full record path (no dir creation).
    fn path_read(pb: &Pillbox, name: &str) -> PathBuf {
        Self::dir_read(pb).join(Self::filename(name))
    }

    /// Read one record by exact name from a single pillbox scope.
    /// Returns `Ok(None)` when the file is missing — callers route
    /// `NotFound` into their "no such X" UX rather than a hard error.
    fn read_one(pb: &Pillbox, name: &str) -> Result<Option<Self::Record>> {
        validate_name(Self::read_action(), name)?;
        let path = Self::path_read(pb, name);
        match fs::read_to_string(&path) {
            Ok(raw) => Self::parse(&raw, &path).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Delete a record. Idempotent — `Ok(())` whether the file existed.
    /// The caller is responsible for any backend resource teardown
    /// (sandbox.kill etc.) before scrubbing the registry entry.
    fn delete(pb: &Pillbox, name: &str) -> Result<bool> {
        validate_name(Self::read_action(), name)?;
        let path = Self::path(pb, name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
        }
    }
}

/// Marker for registries whose reads should walk project→global, with
/// project entries shadowing global ones on key conflict. Implementing
/// this opts the type into [`read_inherited`] and [`list_merged`].
///
/// Sessions deliberately don't implement this — see the module docs
/// for the rationale.
pub(crate) trait InheritedRegistry: Registry {}

/// Walk `resolved.read_chain()` (project → global) for the first scope
/// that has a record with the given name. Returns
/// `(record, source_display_name)` so callers can surface "from <pb>"
/// in `info` / `show` output, or `Ok(None)` if no scope has it.
pub(crate) fn read_inherited<R: InheritedRegistry>(
    resolved: &Pillbox,
    name: &str,
) -> Result<Option<(R::Record, String)>> {
    validate_name(R::read_action(), name)?;
    for pb in resolved.read_chain() {
        let path = R::path_read(&pb, name);
        match fs::read_to_string(&path) {
            Ok(raw) => {
                let rec = R::parse(&raw, &path)?;
                return Ok(Some((rec, pb.display_name().to_string())));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
    Ok(None)
}

/// One entry in the merged listing — the record plus where it came
/// from. Project entries shadow global ones; `from_project` tells
/// `list` callers whether to print `[project]` or `[global]`.
#[derive(Debug, Clone)]
pub(crate) struct MergedEntry<T> {
    /// Logical name of the record (filename minus the registry's
    /// suffix). Exposed for callers that want to render the name
    /// without poking inside the typed `record`; the registry
    /// integration tests rely on it. Not all release-build call sites
    /// dereference it today — kept on the struct because the
    /// alternative is forcing every caller to know the per-record
    /// naming convention.
    #[allow(dead_code)]
    pub(crate) name: String,
    pub(crate) record: T,
    pub(crate) scope: String,
    pub(crate) from_project: bool,
}

/// All records visible from `resolved`, deduplicated by name with
/// project entries shadowing global ones. Names without a matching
/// filename pattern (e.g. sidecar `.meta.json` files for secrets) are
/// filtered by the consumer's `filename`/`name_from_file` impl — see
/// the `MERGED_NAME_FILTER` discussion below.
///
/// **Filename → name decoding**: the trait's `filename(name)` is a
/// one-way function. We invert it heuristically: a record file's name
/// is its full filename minus any extension produced by `filename(""
/// )` — but that's fragile. Instead we accept any filename and let the
/// consumer's `parse` reject mismatches at read time. The merged-entry
/// `name` field is the raw filename with the suffix stripped if the
/// suffix is statically known (`.toml`); secrets / env bundles use
/// the raw filename because their `filename(name)` is just `name`.
pub(crate) fn list_merged<R: InheritedRegistry>(
    resolved: &Pillbox,
) -> Result<Vec<MergedEntry<R::Record>>> {
    let mut map: BTreeMap<String, MergedEntry<R::Record>> = BTreeMap::new();
    for pb in resolved.read_chain() {
        let dir = R::dir_read(&pb);
        if !dir.exists() {
            continue;
        }
        let from_project = matches!(pb.scope, Scope::Project { .. });
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let fname = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Skip files that don't match the registry's filename
            // shape. We discover the suffix by probing `filename("")`
            // — every consumer either appends nothing or a constant
            // suffix (`.toml`), so this matches the live convention.
            let Some(name) = decode_name::<R>(&fname) else {
                continue;
            };
            if map.contains_key(&name) {
                // Already populated from the project (earlier in
                // read_chain) — global must not clobber.
                continue;
            }
            let raw = fs::read_to_string(entry.path())
                .with_context(|| format!("read {}", entry.path().display()))?;
            let record = R::parse(&raw, &entry.path())?;
            map.insert(
                name.clone(),
                MergedEntry {
                    name,
                    record,
                    scope: pb.display_name().to_string(),
                    from_project,
                },
            );
        }
    }
    Ok(map.into_values().collect())
}

/// Inverse of `R::filename`. Returns `None` for filenames that don't
/// belong to this registry (e.g. sidecar `.meta.json` files in
/// `secrets/`). Implemented via probe rather than a separate trait
/// method to keep the API surface small.
fn decode_name<R: Registry>(fname: &str) -> Option<String> {
    // Probe the registry's filename function with an empty string to
    // learn its constant suffix. `filename("")` is `".toml"` for the
    // TOML-suffix registries and `""` for the raw-name registries.
    let suffix = R::filename("");
    if suffix.is_empty() {
        // Raw-name registry (secrets, env): file IS the record. But
        // we still want to filter out things like `.meta.json`
        // sidecars — secrets handles that with an explicit endswith
        // check on its own list path. Here we accept everything; the
        // sidecar filter lives at the call site.
        return Some(fname.to_string());
    }
    fname.strip_suffix(&suffix).map(|s| s.to_string())
}

/// Write a record to its single-scope path. The body is whatever the
/// caller has rendered (TOML, raw value, etc.) — registries that need
/// to serialize from a `Self::Record` should do it at the call site
/// because the rendering can be format-specific (handwritten TOML
/// with comments for remotes, `toml::to_string` for sessions, raw
/// bytes for secrets/env bundles).
pub(crate) fn write_record<R: Registry>(pb: &Pillbox, name: &str, body: &[u8]) -> Result<()> {
    validate_name(R::read_action(), name)?;
    let path = R::path(pb, name)?;
    write_private_file(&path, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pillbox;
    use crate::test_util::with_isolated_home;

    /// Toy registry: stores `String` values, no suffix.
    struct Toy;
    impl Registry for Toy {
        type Record = String;
        const SUBDIR: &'static str = "_toy_registry_test";
        fn read_action() -> &'static str {
            "toy read"
        }
        fn filename(name: &str) -> String {
            name.to_string()
        }
        fn parse(raw: &str, _src: &Path) -> Result<Self::Record> {
            Ok(raw.to_string())
        }
    }
    impl InheritedRegistry for Toy {}

    #[test]
    fn round_trip_via_default_helpers() {
        with_isolated_home("registry-rt", || {
            let g = pillbox::global();
            write_record::<Toy>(&g, "alpha", b"hello").unwrap();
            let v = Toy::read_one(&g, "alpha").unwrap();
            assert_eq!(v.as_deref(), Some("hello"));
            assert!(Toy::delete(&g, "alpha").unwrap());
            assert!(Toy::read_one(&g, "alpha").unwrap().is_none());
            // Idempotent.
            assert!(!Toy::delete(&g, "alpha").unwrap());
        });
    }

    #[test]
    fn list_merged_dedupes_with_project_shadowing_global() {
        with_isolated_home("registry-merged", || {
            let tmp = tempfile::tempdir().unwrap();
            let saved = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            pillbox::new(
                Some("proj".into()),
                None,
                pillbox::NewWorkspaceArgs::default(),
            )
            .unwrap();
            let proj = Pillbox::resolve(None).unwrap();
            let g = pillbox::global();
            write_record::<Toy>(&g, "g_only", b"g").unwrap();
            write_record::<Toy>(&proj, "p_only", b"p").unwrap();
            write_record::<Toy>(&g, "both", b"g-both").unwrap();
            write_record::<Toy>(&proj, "both", b"p-both").unwrap();

            let entries = list_merged::<Toy>(&proj).unwrap();
            let g_only = entries.iter().find(|e| e.name == "g_only").unwrap();
            let p_only = entries.iter().find(|e| e.name == "p_only").unwrap();
            let both = entries.iter().find(|e| e.name == "both").unwrap();
            assert!(!g_only.from_project);
            assert!(p_only.from_project);
            assert!(both.from_project, "project shadows global");
            assert_eq!(both.record, "p-both");

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }

    #[test]
    fn read_inherited_returns_source_scope() {
        with_isolated_home("registry-inherited", || {
            let tmp = tempfile::tempdir().unwrap();
            let saved = std::env::current_dir().ok();
            std::env::set_current_dir(tmp.path()).unwrap();
            pillbox::new(
                Some("proj".into()),
                None,
                pillbox::NewWorkspaceArgs::default(),
            )
            .unwrap();
            let proj = Pillbox::resolve(None).unwrap();
            let g = pillbox::global();
            write_record::<Toy>(&g, "shared", b"global-val").unwrap();

            // Only global has it — proj inherits.
            let (v, src) = read_inherited::<Toy>(&proj, "shared").unwrap().unwrap();
            assert_eq!(v, "global-val");
            assert_eq!(src, "global");

            // Now write at project — project shadows.
            write_record::<Toy>(&proj, "shared", b"proj-val").unwrap();
            let (v, src) = read_inherited::<Toy>(&proj, "shared").unwrap().unwrap();
            assert_eq!(v, "proj-val");
            assert_eq!(src, "proj");

            if let Some(c) = saved {
                let _ = std::env::set_current_dir(c);
            }
        });
    }
}
