//! `pillbox doctor` — diagnose the environment. Always exits 0; the
//! `overall_ok` field (or the ✗ lines in human output) is what callers
//! branch on.
//!
//! Substrate-aware: the checks that gate `overall_ok` are the ones the substrate
//! a real run would select (`SubstrateKind::selected`, same env + build) needs.
//! Whether the runner image is available for rootfs export is reported but never
//! fails the diagnosis.

use std::{os::unix::fs::PermissionsExt, path::PathBuf};

use anyhow::Result;

use crate::pillbox::Pillbox;
use crate::sandbox::oci::{self, RunnerImageSource};
use crate::sandbox::SubstrateKind;

#[derive(Debug)]
pub(crate) struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
    /// Whether this check feeds `overall_ok`. Informational checks (runner image
    /// export readiness, disk headroom, orphan VMMs) are reported but not
    /// required, so they never fail an otherwise-healthy host.
    required: bool,
}

impl Check {
    pub(crate) fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            ok: true,
            detail: detail.into(),
            required: true,
        }
    }
    pub(crate) fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            ok: false,
            detail: detail.into(),
            required: true,
        }
    }
    /// Mark a check as not gating `overall_ok` (reported for visibility only).
    pub(crate) fn optional(mut self) -> Self {
        self.required = false;
        self
    }
}

pub(crate) fn run(json: bool, resolved: &Pillbox) -> Result<()> {
    let selected = SubstrateKind::selected();
    let kind = selected.as_ref().ok().copied();
    let checks = collect_checks(selected, resolved);
    let overall_ok = compute_overall_ok(&checks);
    if json {
        println!("{}", to_json(kind, &checks, overall_ok));
        return Ok(());
    }
    println!(
        "  active backend: {}",
        kind.map_or("(none selected)", SubstrateKind::label)
    );
    for c in &checks {
        let mark = if c.ok { "✓" } else { "✗" };
        println!("  {mark} {:<22} {}", c.name, c.detail);
    }
    println!();
    if overall_ok {
        println!("pillbox: ✓ doctor: all checks passed");
    } else {
        println!("pillbox: doctor found problems. Address the lines marked ✗ above.");
    }
    Ok(())
}

/// Only required checks gate the verdict. A non-required check (an
/// informational warning) never fails overall.
fn compute_overall_ok(checks: &[Check]) -> bool {
    checks.iter().filter(|c| c.required).all(|c| c.ok)
}

fn collect_checks(selected: Result<SubstrateKind>, resolved: &Pillbox) -> Vec<Check> {
    let mut checks = vec![
        check_home_resolvable(),
        check_data_dir_perms(),
        check_substrate(&selected),
    ];
    if let Some(backend) = selected.as_ref().ok().and_then(|kind| kind.backend().ok()) {
        checks.extend(backend.host_checks());
    }
    checks.push(check_runner_image(resolved).optional());
    checks
}

/// Whether the selected substrate can run on this build: `PILLBOX_BACKEND` names
/// one, and this pillbox carries its backend.
fn check_substrate(selected: &Result<SubstrateKind>) -> Check {
    let name = "substrate";
    match selected {
        Err(e) => Check::fail(name, format!("{e:#}")),
        Ok(kind) => match kind.backend() {
            Ok(_) => Check::ok(name, kind.label()),
            Err(e) => Check::fail(name, format!("{e:#}")),
        },
    }
}

fn check_home_resolvable() -> Check {
    match std::env::var("HOME") {
        Ok(h) => Check::ok("home_resolvable", h),
        Err(_) => Check::fail("home_resolvable", "$HOME is not set"),
    }
}

fn check_data_dir_perms() -> Check {
    let name = "data_dir_perms";
    let Ok(home) = std::env::var("HOME") else {
        return Check::fail(name, "cannot check — $HOME unresolved");
    };
    let path = PathBuf::from(home).join(".pillbox");
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Check::ok(
                name,
                format!(
                    "{} will be created on first use (run `pillbox init`)",
                    path.display()
                ),
            );
        }
        Err(e) => {
            return Check::fail(name, format!("could not stat {}: {e}", path.display()));
        }
    };
    let mode = meta.permissions().mode() & 0o777;
    // Strict 0700: even *read* access by group/world leaks the list of
    // authenticated providers and stored secret names.
    if mode & 0o077 != 0 {
        return Check::fail(
            name,
            format!(
                "{} mode {:o} is group/world accessible — run `chmod 700 {}`",
                path.display(),
                mode,
                path.display()
            ),
        );
    }
    // Surface a hint if the v0.5 layout is still around — pillbox itself
    // errors out at command-time, but doctor flagging it here makes the
    // diagnosis surface earlier in onboarding. The list of names that
    // qualify as "legacy" lives in `paths::V0_5_LEGACY_SUBDIRS`.
    let legacy = crate::paths::detect_legacy_subdirs(&path);
    if let Some(first) = legacy.first() {
        return Check::fail(
            name,
            format!(
                "{} mode {mode:o}; legacy v0.5 layout detected (~/.pillbox/{first}/). \
                 Move ~/.pillbox aside and run `pillbox init`.",
                path.display(),
            ),
        );
    }
    Check::ok(name, format!("{} mode {mode:o}", path.display()))
}

/// Whether the runner image the microVM rootfs is exported from is present
/// locally (image plumbing via `docker`; a cached rootfs can boot without it).
fn check_runner_image(resolved: &Pillbox) -> Check {
    let name = "runner_image";
    let (image, source) = oci::resolve_runner_image(resolved);
    let suffix = match source {
        RunnerImageSource::Default => String::new(),
        other => format!(" [from {}]", other.human()),
    };
    match oci::probe_image(&image) {
        Ok(Some(id)) => {
            let id = id
                .trim_start_matches("sha256:")
                .chars()
                .take(12)
                .collect::<String>();
            Check::ok(name, format!("{image} ({id}){suffix}"))
        }
        Ok(None) => Check::fail(
            name,
            format!(
                "{image} not available for rootfs export{suffix} — `docker pull {image}` or set ${}",
                oci::RUNNER_IMAGE_ENV
            ),
        ),
        Err(_) => Check::fail(name, "cannot check — docker CLI unavailable"),
    }
}

fn to_json(kind: Option<SubstrateKind>, checks: &[Check], overall_ok: bool) -> String {
    let arr: Vec<serde_json::Value> = checks
        .iter()
        .map(|c| {
            let mut o = serde_json::Map::new();
            o.insert("name".into(), serde_json::Value::String(c.name.into()));
            o.insert("ok".into(), serde_json::Value::Bool(c.ok));
            o.insert("detail".into(), serde_json::Value::String(c.detail.clone()));
            o.insert("required".into(), serde_json::Value::Bool(c.required));
            serde_json::Value::Object(o)
        })
        .collect();
    crate::paths::json_v1(vec![
        ("backend", kind.map(SubstrateKind::label).into()),
        ("checks", serde_json::Value::Array(arr)),
        ("overall_ok", serde_json::Value::Bool(overall_ok)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overall_ok_ignores_non_required_checks() {
        // The substrate is healthy (required checks pass), but the runner image is
        // absent (a failing, non-required check): overall_ok stays true.
        let checks = vec![
            Check::ok("home_resolvable", "/home/x"),
            Check::ok("virtualization", "available"),
            Check::fail("runner_image", "not found").optional(),
        ];
        assert!(
            compute_overall_ok(&checks),
            "a failing non-required check must not fail overall_ok"
        );
    }

    #[test]
    fn overall_ok_fails_on_a_required_check() {
        let checks = vec![
            Check::ok("home_resolvable", "/home/x"),
            Check::fail("virtualization", "no KVM"),
            Check::ok("runner_image", "present").optional(),
        ];
        assert!(
            !compute_overall_ok(&checks),
            "a failing required check must fail overall_ok"
        );
    }

    #[test]
    fn the_runner_image_check_never_gates_and_a_bad_selection_does() {
        let p = crate::pillbox::global();
        let checks = collect_checks(Ok(SubstrateKind::Managed), &p);
        let image = checks.iter().find(|c| c.name == "runner_image").unwrap();
        assert!(!image.required);
        let substrate = checks.iter().find(|c| c.name == "substrate").unwrap();
        assert!(substrate.ok && substrate.required, "{substrate:?}");

        let refused = check_substrate(&Err(anyhow::anyhow!("PILLBOX_BACKEND=docker was removed")));
        assert!(!refused.ok && refused.required);
        assert!(refused.detail.contains("removed"), "{}", refused.detail);
    }

    #[cfg(not(feature = "libkrun"))]
    #[test]
    fn a_build_without_a_substrate_fails_the_libkrun_selection() {
        let check = check_substrate(&Ok(SubstrateKind::Libkrun));
        assert!(!check.ok && check.required, "{check:?}");
        assert!(!check.detail.contains("docker"), "{}", check.detail);
    }

    #[test]
    fn json_carries_backend_and_required_fields() {
        let checks = vec![Check::ok("virtualization", "available")];
        let json = to_json(Some(SubstrateKind::Libkrun), &checks, true);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["backend"], "libkrun");
        assert_eq!(v["overall_ok"], true);
        assert_eq!(v["checks"][0]["required"], true);
        let none = to_json(None, &checks, false);
        let v: serde_json::Value = serde_json::from_str(&none).unwrap();
        assert!(v["backend"].is_null());
    }
}
