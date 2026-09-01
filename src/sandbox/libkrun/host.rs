//! Host-capability probes for the libkrun backend — pure, side-effect-free
//! checks shared by `doctor` and the launch preflight so "can this host run a
//! microVM" has a single home.
//!
//! Each probe guards a specific, recurring host-side failure that is NOT a
//! pillbox bug:
//! - missing virtualization → the VMM can't open `/dev/kvm` (Linux) or HVF
//!   (macOS), so the VM never boots;
//! - missing runtime dylibs → `brew cleanup`/autoremove sweeps libkrun's
//!   *undeclared* deps (`libepoxy`, `MoltenVK`), and the loader aborts the VMM
//!   with `SIGABRT` at boot — an opaque signal death unless we name the cause;
//! - disk pressure → a half-allocated rootfs/CoW clone stalls the boot, so we
//!   refuse to start below a floor rather than wedge.

use std::path::Path;

/// Minimum free bytes on the filesystem holding the krun cache before a launch is
/// safe. 2 GiB: a materialized runner rootfs plus a CoW workspace clone plus the
/// guest's scratch comfortably fits, with margin so the boot doesn't stall
/// mid-allocation. A floor, not a quota — the caller decides what to do below it.
pub(crate) const MIN_HEADROOM_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// `Ok` if this host's CPU virtualization is usable, else a human reason.
///
/// Linux: the VMM needs `/dev/kvm` present and accessible (read+write). macOS:
/// the Hypervisor framework must report support (`sysctl kern.hv_support == 1`);
/// it is absent on pre-HVF hardware and inside nested/unentitled contexts.
pub(crate) fn virtualization_available() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let dev = Path::new("/dev/kvm");
        if !dev.exists() {
            return Err(
                "/dev/kvm not present — enable KVM (load the kvm module; check BIOS virtualization)"
                    .to_string(),
            );
        }
        // Presence isn't enough: the invoking user must be able to open it
        // read+write (the `kvm` group / device perms). `access(W_OK|R_OK)` is the
        // honest check — `exists()` passes even when perms would deny the open.
        let cpath = std::ffi::CString::new("/dev/kvm").expect("/dev/kvm has no interior NUL");
        let ok = unsafe { libc::access(cpath.as_ptr(), libc::R_OK | libc::W_OK) } == 0;
        if !ok {
            return Err(
                "/dev/kvm not accessible — add your user to the `kvm` group (then re-login)"
                    .to_string(),
            );
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        // `kern.hv_support` is an int sysctl: 1 = HVF available.
        match sysctl_int("kern.hv_support") {
            Some(1) => Ok(()),
            Some(_) => Err(
                "Hypervisor.framework reports no support (kern.hv_support=0) — this Mac/context can't run a microVM"
                    .to_string(),
            ),
            None => Err(
                "could not read kern.hv_support — Hypervisor.framework availability unknown"
                    .to_string(),
            ),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err("libkrun is only supported on Linux (KVM) and macOS (HVF)".to_string())
    }
}

/// `Ok` if libkrun's runtime dylibs resolve, else an `Err` naming the fix.
///
/// libkrun links `libepoxy` and `MoltenVK` at *runtime* without declaring them as
/// package deps, so `brew cleanup`/autoremove silently removes them and the
/// loader `SIGABRT`s the VMM at boot. We stat the standard Homebrew lib dirs for
/// the dylib basenames rather than `dlopen` (cheap, no side effects, safe on a
/// hot path) and rather than shelling out to `brew`.
///
/// Best-effort and biased away from false negatives: this footgun is
/// macOS/Homebrew-specific, so on Linux (and any unknown lib layout) we return
/// `Ok` — a real missing-dep there surfaces as a boot failure, and we'd rather
/// not block a correctly-provisioned host on a probe that doesn't apply.
pub(crate) fn runtime_deps_present() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // `libepoxy.dylib` and `libMoltenVK.dylib` are the load-bearing pair.
        let mut missing = Vec::new();
        if !lib_present("libepoxy") {
            missing.push("libepoxy");
        }
        if !lib_present("libMoltenVK") {
            missing.push("molten-vk");
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "missing libkrun runtime deps ({}) — `brew install {}`",
                missing.join(", "),
                missing.join(" ")
            ))
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        // The `brew cleanup` footgun is macOS-specific; don't risk a false
        // negative on a host whose libs live elsewhere.
        Ok(())
    }
}

/// Bytes free on the filesystem holding `path`, via `statvfs`
/// (`f_bavail * f_frsize`). Returns 0 on stat failure — "no headroom known", the
/// caller decides (treating unknown as a hard block would wedge an otherwise-fine
/// host whose path simply doesn't exist yet).
pub(crate) fn disk_headroom(path: &Path) -> u64 {
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return 0; // interior NUL — unstattable
    };
    // SAFETY: `statvfs` writes a fully-initialized struct on success (rc==0); we
    // read fields only then. `f_bavail`/`f_frsize` widths differ across targets
    // (32-bit on macOS, 64-bit on Linux) — cast both to u64 before multiplying.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut buf) };
    if rc != 0 {
        return 0;
    }
    (buf.f_bavail as u64).saturating_mul(buf.f_frsize as u64)
}

/// Read an integer sysctl by name. `None` if the name is unknown or the read
/// fails, so the caller can distinguish "unknown" from a real `0`.
#[cfg(target_os = "macos")]
fn sysctl_int(name: &str) -> Option<i32> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut value: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    // SAFETY: `oldp` points at `value` with `oldlenp` = its size; on success
    // sysctl writes exactly that int. No new value (`newp`/`newlen` null/0).
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut value as *mut i32 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Some(value)
    } else {
        None
    }
}

/// True if a `<stem>.dylib` exists in any standard Homebrew lib dir.
#[cfg(target_os = "macos")]
fn lib_present(stem: &str) -> bool {
    // Apple-silicon (`/opt/homebrew`) and Intel (`/usr/local`) prefixes; the
    // dylib basename is `<stem>.dylib` (symlinks Homebrew keeps current).
    const LIB_DIRS: [&str; 2] = ["/opt/homebrew/lib", "/usr/local/lib"];
    let file = format!("{stem}.dylib");
    LIB_DIRS
        .iter()
        .any(|dir| Path::new(dir).join(&file).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_headroom_of_cwd_is_nonzero() {
        // The test host has free space on the cwd filesystem.
        let cwd = std::env::current_dir().unwrap();
        assert!(disk_headroom(&cwd) > 0, "expected free space on cwd fs");
    }

    #[test]
    fn disk_headroom_of_missing_path_is_zero() {
        // A non-existent path can't be statted → 0 ("unknown"), not a panic.
        let missing = Path::new("/nonexistent-pillbox-headroom-probe-path");
        assert_eq!(disk_headroom(missing), 0);
    }

    #[test]
    fn probes_return_a_result() {
        // Host-dependent: only exercise that they run and yield a typed Result
        // (don't assert Ok/Err — the CI/dev host may or may not be VM-capable).
        let _: Result<(), String> = virtualization_available();
        let _: Result<(), String> = runtime_deps_present();
    }

    #[test]
    fn min_headroom_is_two_gib() {
        assert_eq!(MIN_HEADROOM_BYTES, 2 * 1024 * 1024 * 1024);
    }
}
