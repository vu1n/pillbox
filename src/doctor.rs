//! `pillbox doctor` — diagnose the environment. Always exits 0; the
//! `overall_ok` field (or the ✗ lines in human output) is what callers
//! branch on.

use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Command, thread};

use anyhow::Result;

use crate::docker;

#[derive(Debug)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            ok: true,
            detail: detail.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Check {
            name,
            ok: false,
            detail: detail.into(),
        }
    }
}

pub(crate) fn run(json: bool) -> Result<()> {
    let checks = collect_checks();
    let overall_ok = checks.iter().all(|c| c.ok);
    if json {
        println!("{}", to_json(&checks, overall_ok));
        return Ok(());
    }
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

fn collect_checks() -> Vec<Check> {
    // Two docker subprocesses each cost ~200ms cold; run them in
    // parallel with the cheap checks.
    let docker_thread = thread::spawn(check_docker_daemon);
    let image_thread = thread::spawn(check_runner_image);
    vec![
        check_home_resolvable(),
        check_data_dir_perms(),
        docker_thread
            .join()
            .unwrap_or_else(|_| Check::fail("docker_daemon", "check thread panicked")),
        image_thread
            .join()
            .unwrap_or_else(|_| Check::fail("runner_image", "check thread panicked")),
    ]
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

fn check_runner_image() -> Check {
    let name = "runner_image";
    match Command::new("docker")
        .args([
            "image",
            "inspect",
            docker::RUNNER_IMAGE,
            "--format",
            "{{.Id}}",
        ])
        .output()
    {
        Ok(out) if out.status.success() => {
            let id = String::from_utf8_lossy(&out.stdout)
                .trim()
                .trim_start_matches("sha256:")
                .chars()
                .take(12)
                .collect::<String>();
            Check::ok(name, format!("{} ({id})", docker::RUNNER_IMAGE))
        }
        Ok(_) => Check::fail(
            name,
            format!(
                "{} not found locally — see pillbox README for image build instructions",
                docker::RUNNER_IMAGE
            ),
        ),
        Err(_) => Check::fail(name, "cannot check — docker CLI unavailable"),
    }
}

fn to_json(checks: &[Check], overall_ok: bool) -> String {
    let arr: Vec<serde_json::Value> = checks
        .iter()
        .map(|c| {
            let mut o = serde_json::Map::new();
            o.insert("name".into(), serde_json::Value::String(c.name.into()));
            o.insert("ok".into(), serde_json::Value::Bool(c.ok));
            o.insert("detail".into(), serde_json::Value::String(c.detail.clone()));
            serde_json::Value::Object(o)
        })
        .collect();
    crate::paths::json_v1(vec![
        ("checks", serde_json::Value::Array(arr)),
        ("overall_ok", serde_json::Value::Bool(overall_ok)),
    ])
}
