//! `pillbox doctor` — diagnose the environment. Always exits 0; the
//! `overall_ok` field (or the ✗ lines in human output) is what callers
//! branch on.
//!
//! Backend-aware: the checks that gate `overall_ok` are the ones the *active*
//! backend actually needs. The active backend is whatever `select_backend`
//! would pick for a real run (same env + build), so doctor stays correct after
//! the default flips between substrates — the non-active backend's absence is
//! reported but never fails the diagnosis.

use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Command, thread};

use anyhow::Result;

use crate::docker::{self, RunnerImageSource};
use crate::pillbox::Pillbox;

#[derive(Debug)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
    /// Whether this check feeds `overall_ok`. A check on a backend that isn't
    /// active (e.g. Docker on a libkrun host) is reported but not required, so
    /// its absence doesn't fail an otherwise-healthy host. Informational
    /// warnings (disk headroom, orphan VMMs) are also non-required.
    required: bool,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            ok: true,
            detail: detail.into(),
            required: true,
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            ok: false,
            detail: detail.into(),
            required: true,
        }
    }
    /// Mark a check as not gating `overall_ok` (reported for visibility only).
    fn optional(mut self) -> Self {
        self.required = false;
        self
    }
}

/// The substrate a real `pillbox run` would use here. Determined by querying
/// `select_backend` — the same decision the run path makes — so doctor's framing
/// follows the active default without re-encoding the selection rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ActiveBackend {
    Docker,
    Libkrun,
}

/// Which backend `select_backend` resolves to for this build + env — read from the
/// backend's own `id()`, so doctor follows the run path's selection without
/// re-encoding the env/feature rule (and without overloading a capability bit as a
/// backend identity, which breaks the moment two backends share that bit).
fn active_backend() -> ActiveBackend {
    if crate::sandbox::select_backend().id() == crate::session::BACKEND_LIBKRUN {
        ActiveBackend::Libkrun
    } else {
        ActiveBackend::Docker
    }
}

pub(crate) fn run(json: bool, resolved: &Pillbox) -> Result<()> {
    let backend = active_backend();
    let checks = collect_checks(backend, resolved);
    let overall_ok = compute_overall_ok(&checks);
    if json {
        println!("{}", to_json(backend, &checks, overall_ok));
        return Ok(());
    }
    println!(
        "  active backend: {}",
        match backend {
            ActiveBackend::Docker => "docker",
            ActiveBackend::Libkrun => "libkrun",
        }
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

/// Only required checks gate the verdict. A non-required check (the inactive
/// backend's compat probe, or an informational warning) never fails overall.
fn compute_overall_ok(checks: &[Check]) -> bool {
    checks.iter().filter(|c| c.required).all(|c| c.ok)
}

fn collect_checks(backend: ActiveBackend, resolved: &Pillbox) -> Vec<Check> {
    let mut checks = vec![check_home_resolvable(), check_data_dir_perms()];
    match backend {
        ActiveBackend::Docker => checks.extend(docker_checks(resolved, true)),
        ActiveBackend::Libkrun => {
            checks.extend(libkrun_checks());
            // Docker still materializes the runner rootfs for libkrun, but a
            // libkrun-default host legitimately runs without it — report its
            // presence without letting its absence fail the diagnosis.
            checks.extend(docker_checks(resolved, false));
        }
    }
    checks
}

/// The Docker daemon + runner-image pair. `required` gates whether they feed
/// `overall_ok` (true when Docker is the active backend; false when it's the
/// optional compat backend behind libkrun). The two docker subprocesses each
/// cost ~200ms cold, so run them in parallel.
fn docker_checks(resolved: &Pillbox, required: bool) -> Vec<Check> {
    let (image, source) = docker::resolve_runner_image(resolved);
    let docker_thread = thread::spawn(check_docker_daemon);
    let image_thread = thread::spawn(move || check_runner_image(&image, source));
    let daemon = docker_thread
        .join()
        .unwrap_or_else(|_| Check::fail("docker_daemon", "check thread panicked"));
    let image = image_thread
        .join()
        .unwrap_or_else(|_| Check::fail("runner_image", "check thread panicked"));
    if required {
        vec![daemon, image]
    } else {
        vec![daemon.optional(), image.optional()]
    }
}

#[cfg(feature = "libkrun")]
fn libkrun_checks() -> Vec<Check> {
    use crate::sandbox::libkrun::{
        disk_headroom, runtime_deps_present, virtualization_available, MIN_HEADROOM_BYTES,
    };

    let virtualization = match virtualization_available() {
        Ok(()) => Check::ok("virtualization", "CPU virtualization available"),
        Err(reason) => Check::fail("virtualization", reason),
    };
    let deps = match runtime_deps_present() {
        Ok(()) => Check::ok("libkrun_deps", "runtime dylibs present"),
        Err(reason) => Check::fail("libkrun_deps", reason),
    };
    // Headroom on the filesystem that holds the krun cache (~/.pillbox/krun);
    // stat the data root, which shares that filesystem and exists earlier. A
    // warning, not a hard fail — the launch preflight is what refuses to boot.
    let headroom = {
        let path = pillbox_data_dir();
        let free = disk_headroom(&path);
        let detail = format!(
            "{} GiB free on {}",
            free / (1024 * 1024 * 1024),
            path.display()
        );
        if free >= MIN_HEADROOM_BYTES {
            Check::ok("disk_headroom", detail).optional()
        } else {
            Check::fail(
                "disk_headroom",
                format!(
                    "{detail} — below the {} GiB floor; free space before launching",
                    MIN_HEADROOM_BYTES / (1024 * 1024 * 1024)
                ),
            )
            .optional()
        }
    };
    vec![virtualization, deps, headroom, check_orphan_vmms()]
}

#[cfg(not(feature = "libkrun"))]
fn libkrun_checks() -> Vec<Check> {
    Vec::new()
}

/// Count of running `__krun-vmm` processes — informational only. The count is
/// host-wide and carries no per-pillbox attribution, so it may include *another*
/// pillbox's live session, not just dead orphans; we deliberately do NOT suggest
/// a host-wide `pkill` (it would kill those live foreign VMs). Never fails the
/// diagnosis.
#[cfg(feature = "libkrun")]
fn check_orphan_vmms() -> Check {
    let name = "orphan_vmms";
    // `pgrep -f` matches the full argv, which carries the `__krun-vmm`
    // subcommand. Exit 0 = matches, 1 = none, >1 = pgrep error/unavailable.
    match Command::new("pgrep").args(["-f", "__krun-vmm"]).output() {
        Ok(out) if out.status.success() => {
            let count = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count();
            if count == 0 {
                Check::ok(name, "none").optional()
            } else {
                Check::fail(
                    name,
                    format!(
                        "{count} `__krun-vmm` process(es) running — may include other pillboxes' \
                         live sessions, not just orphans. Manage owned sessions with `pillbox \
                         session list` / `session rm`; for a confirmed stray, `kill` its specific \
                         pid"
                    ),
                )
                .optional()
            }
        }
        // Exit 1 is pgrep's "no matches" — the clean case.
        Ok(out) if out.status.code() == Some(1) => Check::ok(name, "none").optional(),
        Ok(_) | Err(_) => Check::ok(name, "could not scan (pgrep unavailable)").optional(),
    }
}

/// The filesystem location whose free space governs a libkrun launch — the data
/// root (`~/.pillbox`), same filesystem as the `krun` cache and present earlier.
#[cfg(feature = "libkrun")]
fn pillbox_data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".pillbox")
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

fn check_docker_daemon() -> Check {
    let name = "docker_daemon";
    match Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
    {
        Ok(out) if out.status.success() => Check::ok(
            name,
            format!("Docker {}", String::from_utf8_lossy(&out.stdout).trim()),
        ),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            let detail = if stderr.contains("Cannot connect to the Docker daemon") {
                "daemon not running — start Docker Desktop".into()
            } else {
                stderr
            };
            Check::fail(name, detail)
        }
        Err(e) => Check::fail(name, format!("docker CLI not found: {e}")),
    }
}

fn check_runner_image(image: &str, source: RunnerImageSource) -> Check {
    let name = "runner_image";
    let suffix = match source {
        RunnerImageSource::Default => String::new(),
        other => format!(" [from {}]", other.human()),
    };
    match Command::new("docker")
        .args(["image", "inspect", image, "--format", "{{.Id}}"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let id = String::from_utf8_lossy(&out.stdout)
                .trim()
                .trim_start_matches("sha256:")
                .chars()
                .take(12)
                .collect::<String>();
            Check::ok(name, format!("{image} ({id}){suffix}"))
        }
        Ok(_) => Check::fail(
            name,
            format!(
                "{image} not found locally{suffix} — `docker pull {image}` or set ${}",
                docker::RUNNER_IMAGE_ENV
            ),
        ),
        Err(_) => Check::fail(name, "cannot check — docker CLI unavailable"),
    }
}

fn to_json(backend: ActiveBackend, checks: &[Check], overall_ok: bool) -> String {
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
    let backend = match backend {
        ActiveBackend::Docker => "docker",
        ActiveBackend::Libkrun => "libkrun",
    };
    crate::paths::json_v1(vec![
        ("backend", serde_json::Value::String(backend.into())),
        ("checks", serde_json::Value::Array(arr)),
        ("overall_ok", serde_json::Value::Bool(overall_ok)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overall_ok_ignores_non_required_checks() {
        // The active backend is healthy (required checks pass), but the optional
        // compat backend is absent (a failing, non-required check). overall_ok
        // must stay true — an absent inactive backend never fails the diagnosis.
        let checks = vec![
            Check::ok("home_resolvable", "/home/x"),
            Check::ok("virtualization", "available"),
            Check::fail("docker_daemon", "daemon not running").optional(),
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
            Check::ok("docker_daemon", "present").optional(),
        ];
        assert!(
            !compute_overall_ok(&checks),
            "a failing required check must fail overall_ok"
        );
    }

    #[test]
    fn docker_checks_are_required_when_active_optional_when_compat() {
        let p = crate::pillbox::global();
        let required = docker_checks(&p, true);
        assert!(
            required.iter().all(|c| c.required),
            "active docker checks must be required"
        );
        let compat = docker_checks(&p, false);
        assert!(
            compat.iter().all(|c| !c.required),
            "compat docker checks must be non-required"
        );
    }

    #[cfg(feature = "libkrun")]
    #[test]
    fn libkrun_active_with_docker_absent_still_passes_when_libkrun_ok() {
        // Simulate a libkrun-default host with no Docker: the libkrun checks pass
        // (required) and the docker compat checks fail (non-required) — overall_ok
        // must remain true.
        let checks = vec![
            Check::ok("home_resolvable", "/home/x"),
            Check::ok("data_dir_perms", "700"),
            Check::ok("virtualization", "available"),
            Check::ok("libkrun_deps", "present"),
            Check::ok("disk_headroom", "100 GiB free").optional(),
            Check::ok("orphan_vmms", "none").optional(),
            Check::fail("docker_daemon", "not running").optional(),
            Check::fail("runner_image", "not found").optional(),
        ];
        assert!(
            compute_overall_ok(&checks),
            "libkrun-active host without Docker must still pass"
        );
    }

    #[cfg(feature = "libkrun")]
    #[test]
    fn json_carries_backend_and_required_fields() {
        let checks = vec![Check::ok("virtualization", "available")];
        let json = to_json(ActiveBackend::Libkrun, &checks, true);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["backend"], "libkrun");
        assert_eq!(v["overall_ok"], true);
        assert_eq!(v["checks"][0]["required"], true);
    }
}
