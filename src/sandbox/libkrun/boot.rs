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

/// Set up the boot channel: write `content` as the boot script into `dir`
/// (normalized to end in exactly one newline) and return the [`Share`] exposing
/// `dir` under `tag` together with the static-ASCII exec that mounts it at
/// `mountpoint` and runs the script.
pub(super) fn boot_channel(
    dir: &Path,
    tag: &str,
    mountpoint: &str,
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
    Ok((
        Share {
            tag: tag.to_string(),
            host_path: dir.to_string_lossy().into_owned(),
        },
        bootstrap_exec(tag, mountpoint),
    ))
}

/// The static kernel-cmdline bootstrap: mount `tag` at `mountpoint`, exec the
/// boot script from it.
fn bootstrap_exec(tag: &str, mountpoint: &str) -> Vec<String> {
    let mp = shell_quote(mountpoint);
    vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "set -e; mkdir -p {mp}; mount -t virtiofs {tag} {mp}; exec /bin/sh {mp}/{BOOT_SCRIPT}"
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
        (
            "PATH".into(),
            format!("/usr/local/bin:/usr/bin:/bin:{GUEST_HOME}/.local/bin"),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bootstrap argv AND the static child env ride the kernel cmdline —
    /// libkrun validates both as printable ASCII (`' '..='~'`) and a violation
    /// aborts the VMM. Everything dynamic must stay out of them; this pins the
    /// invariant for both halves.
    #[test]
    fn kernel_cmdline_parts_stay_printable_ascii() {
        let ascii = |s: &str| s.chars().all(|c| matches!(c, ' '..='~'));
        for part in bootstrap_exec("creds", GUEST_HOME) {
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
    }

    /// boot_channel binds script location, share tag, and exec mount together —
    /// the drift the constructor exists to prevent — and normalizes the
    /// trailing newline.
    #[test]
    fn boot_channel_binds_share_to_exec_and_writes_script() {
        let dir = tempfile::tempdir().unwrap();
        let (share, exec) = boot_channel(dir.path(), "creds", "/root", "echo hi").unwrap();
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
        boot_channel(dir.path(), "creds", "/root", "echo hi").unwrap();
        let mode = std::fs::metadata(dir.path().join(BOOT_SCRIPT))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "boot script must be 0600");
    }
}
