//! The sealed OpenCode text VM shares ownership/materialization with repository
//! VMs, but has no file broker, host shares, persistent home or OAuth rotation.

use super::*;
use crate::agents::harness::opencode::{Catalog, Profile, BRIDGE, IMAGE_METADATA};

pub(crate) struct Image {
    runtime: TempDir,
    rootfs: PathBuf,
    pub(crate) catalog: Catalog,
}

pub(crate) fn image_id(
    pb: &crate::pillbox::Pillbox,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<String> {
    let (image, _) = crate::docker::resolve_runner_image(pb);
    let bytes = run_command(
        Command::new("docker").args(["image", "inspect", &image, "--format", "{{.Id}}"]),
        deadline,
        cancelled,
        "inspect immutable runner image",
    )?;
    let id = String::from_utf8(bytes)?.trim().to_owned();
    validate_image_id(&id)?;
    Ok(id)
}

pub(crate) fn resolve_image(
    image_id: &str,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<Image> {
    validate_image_id(image_id)?;
    let (runtime, rootfs) = prepare_rootfs(image_id, deadline, cancelled)?;
    let path = rootfs.join(IMAGE_METADATA.trim_start_matches('/'));
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .context("runner image has no OpenCode text catalog; rebuild the runner")?;
    ensure!(
        file.metadata()?.is_file(),
        "image catalog is not a regular file"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 4 * 1024 * 1024, "image catalog too large");
    let catalog = serde_json::from_slice(&bytes).context("invalid OpenCode image catalog")?;
    Ok(Image {
        runtime,
        rootfs,
        catalog,
    })
}

/// The real key has no Debug/Serialize representation and only reaches the
/// host VMM on stdin. Guest files contain the per-invocation stub exclusively.
pub(crate) fn launch(
    image: Image,
    profile: &Profile,
    real: String,
    limits: VmLimits,
    cancelled: &dyn Fn() -> bool,
    stage: &mut dyn FnMut(&'static str),
) -> Result<OwnedVm> {
    limits.validate()?;
    ensure!(
        !real.is_empty() && real.len() <= 64 * 1024,
        "invalid provider credential"
    );
    let stub = format!("pillbox-text-{}", uuid::Uuid::now_v7());
    ensure!(real != stub, "invalid provider credential");
    let deadline = Instant::now()
        .checked_add(limits.max_duration)
        .context("VM deadline overflow")?;
    let ca_dir = image.runtime.path().join("ca");
    fs::create_dir(&ca_dir)?;
    crate::paths::ensure_mode_0700(&ca_dir)?;
    let ca = crate::vault::Ca::ensure(&ca_dir).map_err(anyhow::Error::msg)?;
    for directory in [GUEST_HOME, GUEST_RUNTIME, "/workspace", "/tmp"] {
        fresh_directory(&image.rootfs, directory)?;
    }
    let runtime = image.rootfs.join(GUEST_RUNTIME.trim_start_matches('/'));
    write_private_file(&runtime.join("bridge.py"), BRIDGE.as_bytes())?;
    write_private_file(&runtime.join("ca.crt"), &fs::read(ca.cert_path())?)?;
    let credentials = serde_json::to_vec(&serde_json::json!({profile.credential_ref: stub}))?;
    ensure!(
        !credentials
            .windows(real.len())
            .any(|part| part == real.as_bytes()),
        "real credential in guest file"
    );
    write_private_file(&runtime.join("credentials.json"), &credentials)?;
    write_private_file(
        &runtime.join("limits.json"),
        &serde_json::to_vec(&serde_json::json!({
        "duration_ms": remaining_ms(deadline)?,
        "max_output_bytes": limits.max_output_bytes, "max_frame_bytes": limits.max_frame_bytes}))?,
    )?;
    prepare_generated_metadata(
        &image.rootfs,
        &[GUEST_RUNTIME, GUEST_HOME, "/workspace", "/tmp"],
    )?;
    stage("guest_prepare");
    let mut vm = launch_prepared(
        image.runtime,
        limits,
        deadline,
        cancelled,
        true,
        |owner, rpc, remaining| spec(&image.rootfs, &ca_dir, owner, rpc, profile.host, remaining),
    )?;
    let delivery = (|| -> Result<()> {
        let bytes = serde_json::to_vec(&vec![SwapPair {
            stub,
            real,
            hosts: vec![profile.host.into()],
        }])?;
        let mut stdin = vm
            .process
            .child
            .stdin
            .take()
            .context("VMM credential channel missing")?;
        nonblocking(stdin.as_raw_fd())?;
        write_until(&mut stdin, &bytes, deadline, cancelled)?;
        Ok(())
    })();
    if let Err(error) = delivery {
        let cleanup = vm.stop_and_reap();
        return Err(cleanup_failure(error, cleanup));
    }
    stage("vmm_spawn");
    Ok(vm)
}

fn spec(
    rootfs: &Path,
    ca_dir: &Path,
    owner: &Path,
    rpc: &Path,
    host: &str,
    remaining_ms: u64,
) -> VmSpec {
    VmSpec {
        rootfs: rootfs.to_string_lossy().into_owned(),
        vcpus: 2,
        ram_mib: 2048,
        shares: vec![],
        exec: vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-S".into(),
            format!("{GUEST_RUNTIME}/bridge.py"),
        ],
        vsock: Some(VsockAttach {
            port: RPC_PORT,
            host_sock: rpc.to_string_lossy().into_owned(),
            listen: false,
        }),
        egress: Some(EgressSpec {
            allowlist: vec![host.into()],
            log_path: None,
            ca_dir: Some(ca_dir.to_string_lossy().into_owned()),
            local_forward_port: None,
            refresh: None,
        }),
        ownership: Some(OwnershipSpec {
            socket: owner.to_string_lossy().into_owned(),
            remaining_ms: Some(remaining_ms),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_host_shares_and_only_vault_provider_egress() {
        let spec = spec(
            Path::new("/private/rootfs"),
            Path::new("/private/ca"),
            Path::new("/private/owner"),
            Path::new("/private/rpc"),
            "api.openai.com",
            10_000,
        );
        assert!(spec.shares.is_empty());
        let egress = spec.egress.unwrap();
        assert_eq!(egress.allowlist, ["api.openai.com"]);
        assert!(egress.ca_dir.is_some());
        assert!(egress.local_forward_port.is_none());
        assert!(egress.refresh.is_none());
        assert!(spec.ownership.is_some());
    }
}
