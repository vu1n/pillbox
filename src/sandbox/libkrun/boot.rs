//! The guest boot channel — the one guest-bound path that may carry arbitrary
//! bytes.
//!
//! libkrun serializes the guest exec argv and env into the kernel cmdline, which
//! accepts printable ASCII only — one newline or non-ASCII byte (a seeded
//! prompt, a `--memory` briefing, an env value, a workspace name) aborts the VMM
//! with `InvalidAscii`. So the cmdline carries only a fixed prologue — mount a
//! share, exec the boot script from it — and every dynamic byte lives in the
//! script file, where anything is legal. [`boot_channel`] is the only way to set
//! the channel up: it writes the script and returns the matched virtio-fs
//! [`Share`] + exec argv as one unit, so the share tag, mount point, and script
//! location can't drift apart across call sites. Everything that does ride the
//! cmdline must stay printable-ASCII-pure (tested).

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::{shell_quote, Share};
use crate::agents::GUEST_HOME;

/// In-share filename of the host-written guest boot script.
const BOOT_SCRIPT: &str = ".pillbox-boot.sh";

/// Whether the mounted boot share is a disposable per-run clone whose ownership
/// must be normalized for the guest's nested user namespace. `PreserveHost` is
/// for non-clone shares such as the grader's temporary boot-script directory.
#[derive(Clone, Copy)]
pub(super) enum MountedShareOwnership {
    PreserveHost,
    GuestRootClone,
}

/// Set up the boot channel: write `content` as the boot script into `dir`
/// (normalized to end in exactly one newline) and return the [`Share`] exposing
/// `dir` under `tag` together with the static-ASCII exec that mounts it at
/// `mountpoint` and runs the script.
pub(super) fn boot_channel(
    dir: &Path,
    tag: &str,
    mountpoint: &str,
    ownership: MountedShareOwnership,
    content: &str,
) -> Result<(Share, Vec<String>)> {
    let script = format!("{}\n", content.trim_end_matches('\n'));
    // The script can carry plaintext secret values (env exports) — create it
    // owner-only from the start rather than chmod after the fact.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join(BOOT_SCRIPT))
        .and_then(|mut f| f.write_all(script.as_bytes()))
        .context("write guest boot script")?;
    #[cfg(target_os = "macos")]
    if matches!(ownership, MountedShareOwnership::GuestRootClone) {
        super::metadata::prepare_guest_clone_metadata(dir)?;
    }
    Ok((
        Share {
            tag: tag.to_string(),
            host_path: dir.to_string_lossy().into_owned(),
        },
        bootstrap_exec(tag, mountpoint, ownership),
    ))
}

/// Render the legacy ownership normalization for one mounted disposable clone.
/// macOS prepares the clone's virtio-fs metadata on the host before mounting;
/// this remains the guest-side path for non-macOS libkrun backends.
#[cfg(not(target_os = "macos"))]
pub(super) fn guest_root_clone_ownership(mountpoint: &str) -> String {
    format!(
        "find -P {} -xdev -exec /bin/sh -c 'for path do \
         mode=$(stat -c %a -- \"$path\") || exit; \
         chown -h 0:0 -- \"$path\" || exit; \
         if [ ! -L \"$path\" ]; then chmod \"$mode\" -- \"$path\" || exit; fi; \
         done' sh {{}} +",
        shell_quote(mountpoint)
    )
}

/// The static kernel-cmdline bootstrap: mount `tag` at `mountpoint`, exec the
/// boot script from it.
fn bootstrap_exec(tag: &str, mountpoint: &str, ownership: MountedShareOwnership) -> Vec<String> {
    let mp = shell_quote(mountpoint);
    let normalize = match ownership {
        MountedShareOwnership::PreserveHost => String::new(),
        MountedShareOwnership::GuestRootClone => {
            #[cfg(target_os = "macos")]
            {
                String::new()
            }
            #[cfg(not(target_os = "macos"))]
            {
                format!("{}; ", guest_root_clone_ownership(mountpoint))
            }
        }
    };
    vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "set -e; mkdir -p {mp}; mount -t virtiofs {tag} {mp}; {normalize}exec /bin/sh {mp}/{BOOT_SCRIPT}"
        ),
    ]
}

/// `export K='v'` lines for the boot script — how the composed guest env reaches
/// the agent now that the cmdline can't carry it. Values are shell-quoted (any
/// byte is legal); keys are spliced unquoted, so reject anything that isn't a
/// plain identifier rather than let a hostile name escape into the script.
/// Secret/bundle names may legally carry `-`/`.`, so point the user at the
/// `--with NAME=ENV_VAR` rename rather than dead-ending them.
pub(super) fn env_exports(env: &[(String, String)]) -> Result<String> {
    let mut out = String::new();
    for (k, v) in env {
        if !crate::envs::is_valid_env_key(k) {
            bail!(
                "guest env var name {k:?} can't ride the boot script (must be a shell identifier) — inject it under a different name: --with '{k}=SOME_NAME'"
            );
        }
        out.push_str(&format!("export {k}={}\n", shell_quote(v)));
    }
    Ok(out)
}

/// What the VMM child process is spawned with — and therefore all the kernel
/// cmdline ever carries beyond the bootstrap: the static ASCII base. The full
/// guest env travels in the boot script's exports instead.
pub(super) fn static_child_env() -> Vec<(String, String)> {
    vec![
        ("HOME".into(), GUEST_HOME.into()),
        ("TERM".into(), "xterm-256color".into()),
        ("PATH".into(), crate::agents::guest_path()),
    ]
}

/// The grader VM's base env (`session score --in-sandbox`). Deliberately diverges
/// from [`static_child_env`] on one axis: `HOME=/root`, because the grader mounts
/// no creds share (it holds no credentials) so there's no `/home/pillbox` to point
/// at. It shares the agent's `PATH` on purpose — a verifier must resolve the same
/// `~/.local/bin` toolchain the agent used, or a tool that worked at agent time
/// fails the grade. All-static-ASCII, so it's safe on the kernel cmdline; a future
/// *dynamic* grader env value would have to move to the boot-script exports (the
/// agent paths' shape) to dodge `InvalidAscii`.
pub(super) fn grader_child_env() -> Vec<(&'static str, String)> {
    vec![
        ("HOME", "/root".into()),
        ("TERM", "xterm-256color".into()),
        ("PATH", crate::agents::guest_path()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    fn write_test_tool(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;

        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(target_os = "macos"))]
    fn ownership_test_tools(dir: &Path) {
        write_test_tool(
            dir,
            "stat",
            "#!/bin/sh\nfor path do :; done\ncase \"$path\" in */setid-tool) printf '6751\\n'; exit 0;; esac\nif /usr/bin/stat -c %a -- \"$path\" >/dev/null 2>&1; then\n  exec /usr/bin/stat -c %a -- \"$path\"\nfi\nexec /usr/bin/stat -f %Lp \"$path\"\n",
        );
        write_test_tool(
            dir,
            "chown",
            "#!/bin/sh\nfor path do :; done\nprintf '%s\\n' \"$path\" >> \"$OWN_LOG\" || exit\nif [ \"${FAIL_PATH:-}\" = \"$path\" ]; then exit 42; fi\n",
        );
        write_test_tool(
            dir,
            "chmod",
            "#!/bin/sh\nmode=$1\nfor path do :; done\nprintf '%s %s\\n' \"$mode\" \"$path\" >> \"$CHMOD_LOG\"\n",
        );
    }

    /// The bootstrap argv AND the static child env ride the kernel cmdline —
    /// libkrun validates both as printable ASCII (`' '..='~'`) and a violation
    /// aborts the VMM. Everything dynamic must stay out of them; this pins the
    /// invariant for both halves.
    #[test]
    fn kernel_cmdline_parts_stay_printable_ascii() {
        let ascii = |s: &str| s.chars().all(|c| matches!(c, ' '..='~'));
        for part in bootstrap_exec("creds", GUEST_HOME, MountedShareOwnership::GuestRootClone) {
            assert!(
                ascii(&part),
                "bootstrap exec must stay printable ASCII: {part:?}"
            );
        }
        for (k, v) in static_child_env() {
            assert!(
                ascii(&k) && ascii(&v),
                "static child env must stay printable ASCII: {k}={v:?}"
            );
        }
        // The grader env rides the cmdline too (it spawns with these, not via the
        // boot script) — pin it the same way.
        for (k, v) in grader_child_env() {
            assert!(
                ascii(k) && ascii(&v),
                "grader child env must stay printable ASCII: {k}={v:?}"
            );
        }
    }

    /// boot_channel binds script location, share tag, and exec mount together —
    /// the drift the constructor exists to prevent — and normalizes the
    /// trailing newline.
    #[test]
    fn boot_channel_binds_share_to_exec_and_writes_script() {
        let dir = tempfile::tempdir().unwrap();
        let (share, exec) = boot_channel(
            dir.path(),
            "creds",
            "/root",
            MountedShareOwnership::GuestRootClone,
            "echo hi",
        )
        .unwrap();
        assert_eq!(share.tag, "creds");
        assert_eq!(share.host_path, dir.path().to_string_lossy());
        assert!(exec
            .last()
            .unwrap()
            .contains("mount -t virtiofs creds '/root'"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(BOOT_SCRIPT)).unwrap(),
            "echo hi\n"
        );
    }

    #[test]
    fn cloned_boot_share_is_owned_after_mount_before_script_exec() {
        let command = bootstrap_exec("creds", GUEST_HOME, MountedShareOwnership::GuestRootClone)
            .pop()
            .unwrap();
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                command,
                "set -e; mkdir -p '/home/pillbox'; mount -t virtiofs creds '/home/pillbox'; exec /bin/sh '/home/pillbox'/.pillbox-boot.sh"
            );
            assert!(!command.contains("chown"));
        }
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            command,
            "set -e; mkdir -p '/home/pillbox'; mount -t virtiofs creds '/home/pillbox'; find -P '/home/pillbox' -xdev -exec /bin/sh -c 'for path do mode=$(stat -c %a -- \"$path\") || exit; chown -h 0:0 -- \"$path\" || exit; if [ ! -L \"$path\" ]; then chmod \"$mode\" -- \"$path\" || exit; fi; done' sh {} +; exec /bin/sh '/home/pillbox'/.pillbox-boot.sh"
        );
        #[cfg(not(target_os = "macos"))]
        assert!(command.contains("mode=$(stat -c %a"));
        #[cfg(not(target_os = "macos"))]
        assert!(command.contains("chmod \"$mode\""));
    }

    #[test]
    fn non_clone_boot_share_preserves_host_ownership() {
        let command = bootstrap_exec(
            "boot",
            "/run/pillbox-boot",
            MountedShareOwnership::PreserveHost,
        )
        .pop()
        .unwrap();
        assert!(!command.contains("chown"));
        assert!(command.contains("mount -t virtiofs boot '/run/pillbox-boot'; exec"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn clone_ownership_is_physical_no_dereference_and_shell_quoted() {
        let command = guest_root_clone_ownership("/workspace/a b'; touch /escaped");
        assert_eq!(
            command,
            "find -P '/workspace/a b'\\''; touch /escaped' -xdev -exec /bin/sh -c 'for path do mode=$(stat -c %a -- \"$path\") || exit; chown -h 0:0 -- \"$path\" || exit; if [ ! -L \"$path\" ]; then chmod \"$mode\" -- \"$path\" || exit; fi; done' sh {} +"
        );
        assert!(command.starts_with("find -P "));
        assert!(command.contains(" -xdev "));
        assert!(command.contains("chown -h 0:0"));
        assert!(!command.contains("find -L"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn clone_ownership_restores_captured_special_modes_without_following_symlinks() {
        use std::os::unix::fs::symlink;

        // This host's test sandbox strips setid bits from fixtures. The fake
        // `stat` therefore reports 6751 for `setid-tool`, and the fake `chmod`
        // records the mode it receives: the guest VM probe covers the real GNU
        // filesystem behavior while this pins capture -> exact restore wiring.
        let fixture = tempfile::tempdir().unwrap();
        let tools = fixture.path().join("tools");
        let clone = fixture.path().join("clone");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        ownership_test_tools(&tools);

        let executable = clone.join("setid-tool");
        let outside_file = outside.join("must-not-be-visited");
        std::fs::write(&executable, b"fixture").unwrap();
        std::fs::write(&outside_file, b"outside").unwrap();
        symlink(&outside, clone.join("escape")).unwrap();
        let own_log = fixture.path().join("owned.log");
        let chmod_log = fixture.path().join("chmod.log");

        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(guest_root_clone_ownership(clone.to_str().unwrap()))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/sbin", tools.display()),
            )
            .env("OWN_LOG", &own_log)
            .env("CHMOD_LOG", &chmod_log)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            std::fs::read_to_string(chmod_log)
                .unwrap()
                .contains(&format!("6751 {}", executable.display())),
            "the exact captured mode, including setuid/setgid, must reach chmod"
        );
        let owned = std::fs::read_to_string(own_log).unwrap();
        assert!(owned.contains(clone.join("escape").to_str().unwrap()));
        assert!(!owned.contains(outside_file.to_str().unwrap()));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn clone_ownership_child_failure_aborts_the_preamble() {
        let fixture = tempfile::tempdir().unwrap();
        let tools = fixture.path().join("tools");
        let clone = fixture.path().join("clone");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir_all(&clone).unwrap();
        ownership_test_tools(&tools);
        let own_log = fixture.path().join("owned.log");
        let chmod_log = fixture.path().join("chmod.log");

        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "set -e; {}; printf SHOULD_NOT_RUN",
                guest_root_clone_ownership(clone.to_str().unwrap())
            ))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/sbin", tools.display()),
            )
            .env("OWN_LOG", &own_log)
            .env("CHMOD_LOG", &chmod_log)
            .env("FAIL_PATH", &clone)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SHOULD_NOT_RUN"));
    }

    /// The whole point of the boot script: bytes the cmdline can't carry
    /// (newlines, unicode — a seeded prompt, a --memory briefing) survive
    /// verbatim inside the quoted export.
    #[test]
    fn env_exports_carries_any_byte_in_values() {
        let out = env_exports(&[("SEED".into(), "multi\nline — émoji".into())]).unwrap();
        assert_eq!(out, "export SEED='multi\nline — émoji'\n");
    }

    #[test]
    fn env_exports_quotes_embedded_single_quotes() {
        let out = env_exports(&[("K".into(), "it's".into())]).unwrap();
        assert_eq!(out, "export K='it'\\''s'\n");
    }

    /// Keys are spliced unquoted into the script — a non-identifier name is a
    /// script-injection vector, so it must be rejected, not escaped. The
    /// rejection must carry the `--with NAME=ENV_VAR` rename hint: legal
    /// secret names (with `-`/`.`) land here, and the rename is the way out.
    #[test]
    fn env_exports_rejects_non_identifier_keys() {
        for bad in ["BAD KEY", "9LEAD", "INJ'ECT", "", "A=B"] {
            let err = env_exports(&[(bad.to_string(), "v".into())])
                .expect_err(&format!("key {bad:?} must be rejected"));
            assert!(
                err.to_string().contains("--with"),
                "rejection for {bad:?} must carry the --with rename hint, got: {err}"
            );
        }
    }

    /// The boot script can carry plaintext secret values in its exports, so
    /// it must be created owner-only — never readable via the share dir.
    #[test]
    fn boot_channel_writes_script_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        boot_channel(
            dir.path(),
            "creds",
            "/root",
            MountedShareOwnership::GuestRootClone,
            "echo hi",
        )
        .unwrap();
        let mode = std::fs::metadata(dir.path().join(BOOT_SCRIPT))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "boot script must be 0600");
    }
}
