//! Pi-only guest preparation using the bounded repository VM ownership machinery.

use super::*;
use crate::execution::pi_text::{self as protocol, Credentials};

pub(crate) fn image_id(
    pb: &crate::pillbox::Pillbox,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<String> {
    let (image, _) = crate::docker::resolve_runner_image(pb);
    // Docker supplies OCI image bytes/identity only; execution is always libkrun.
    let bytes = run_command(
        Command::new("docker").args(["image", "inspect", &image, "--format", "{{.Id}}"]),
        deadline,
        cancelled,
        "inspect Pi runner image",
    )?;
    let id = String::from_utf8(bytes)?.trim().to_owned();
    validate_image_id(&id)?;
    Ok(id)
}

pub(crate) fn launch(
    image: &str,
    credentials: Option<&Credentials>,
    refresh_path: Option<&Path>,
    limits: VmLimits,
    cancelled: &dyn Fn() -> bool,
    stage: &mut dyn FnMut(&'static str),
) -> Result<OwnedVm> {
    limits.validate()?;
    validate_image_id(image)?;
    ensure!(
        credentials.is_some() == refresh_path.is_some(),
        "Pi refresh path mismatch"
    );
    if let Some(path) = refresh_path {
        ensure!(path.is_absolute(), "Pi refresh path must be absolute");
    }
    if let Some(credentials) = credentials {
        for release in [&credentials.access, &credentials.account] {
            ensure!(
                !release.real.is_empty()
                    && !release.stub.is_empty()
                    && release.real != release.stub,
                "Pi credential release invalid"
            );
        }
    }
    let deadline = Instant::now()
        .checked_add(limits.max_duration)
        .context("Pi VM deadline overflow")?;
    let (runtime, rootfs) = prepare_rootfs(image, deadline, cancelled)?;
    stage("image_prepare");
    let ca_dir = runtime.path().join("ca");
    fs::create_dir(&ca_dir)?;
    crate::paths::ensure_mode_0700(&ca_dir)?;
    let ca = crate::vault::Ca::ensure(&ca_dir).map_err(anyhow::Error::msg)?;
    let certificate = fs::read(ca.cert_path())?;
    prepare_guest(
        &rootfs,
        credentials.is_some(),
        &certificate,
        limits,
        remaining_ms(deadline)?,
    )?;
    stage("guest_prepare");
    let mut vm = launch_prepared(
        runtime,
        limits,
        deadline,
        cancelled,
        credentials.is_some(),
        |owner, rpc, remaining| {
            spec(
                &rootfs,
                &ca_dir,
                owner,
                rpc,
                credentials,
                refresh_path,
                remaining,
            )
        },
    )?;
    if let Some(credentials) = credentials {
        let delivery = (|| -> Result<()> {
            let swaps: Vec<_> = [&credentials.access, &credentials.account]
                .iter()
                .map(|release| SwapPair {
                    stub: release.stub.clone(),
                    real: release.real.clone(),
                    hosts: vec![protocol::PROVIDER_HOST.into()],
                })
                .collect();
            let bytes = serde_json::to_vec(&swaps)?;
            let mut stdin = vm
                .process
                .child
                .stdin
                .take()
                .context("Pi VMM credential channel absent")?;
            nonblocking(stdin.as_raw_fd())?;
            write_until(&mut stdin, &bytes, deadline, cancelled)?;
            Ok(())
        })();
        if let Err(error) = delivery {
            let cleanup = vm.stop_and_reap();
            return Err(cleanup_failure(error, cleanup));
        }
    }
    stage("vmm_spawn");
    Ok(vm)
}

#[allow(clippy::too_many_arguments)]
fn spec(
    rootfs: &Path,
    ca: &Path,
    owner: &Path,
    rpc: &Path,
    credentials: Option<&Credentials>,
    refresh: Option<&Path>,
    remaining_ms: u64,
) -> VmSpec {
    // Context: doc://pillbox/adr-001-libkrun-is-the-backend@0001#libkrun-is-the-backend — all Pi text execution uses the owned microVM.
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
        egress: credentials
            .zip(refresh)
            .map(|(credential, path)| EgressSpec {
                allowlist: vec![protocol::PROVIDER_HOST.into()],
                log_path: None,
                ca_dir: Some(ca.to_string_lossy().into_owned()),
                local_forward_port: None,
                refresh: Some(RefreshSpec {
                    creds_path: path.to_string_lossy().into_owned(),
                    auth_id: "codex".into(),
                    access_stub: credential.access.stub.clone(),
                }),
            }),
        ownership: Some(OwnershipSpec {
            socket: owner.to_string_lossy().into_owned(),
            remaining_ms: Some(remaining_ms),
        }),
    }
}

fn prepare_guest(
    rootfs: &Path,
    turn: bool,
    certificate: &[u8],
    limits: VmLimits,
    remaining_ms: u64,
) -> Result<()> {
    for path in [GUEST_HOME, GUEST_RUNTIME, "/workspace", "/tmp"] {
        fresh_directory(rootfs, path)?;
    }
    // No host home, repository, MCP configuration, or persistent Pi state is shared.
    let agent_dir = rootfs.join("home/pillbox/.pi/agent");
    fs::create_dir_all(&agent_dir)?;
    crate::paths::ensure_mode_0700(&agent_dir)?;
    let runtime = rootfs.join(GUEST_RUNTIME.trim_start_matches('/'));
    write_private_file(&runtime.join("bridge.py"), protocol::BRIDGE.as_bytes())?;
    write_private_file(&runtime.join("driver.mjs"), protocol::DRIVER.as_bytes())?;
    write_private_file(&runtime.join("ca.crt"), certificate)?;
    write_private_file(
        &runtime.join("mode.json"),
        &serde_json::to_vec(if turn { "turn" } else { "resolve" })?,
    )?;
    write_private_file(
        &runtime.join("limits.json"),
        &serde_json::to_vec(&serde_json::json!({
            "duration_ms": remaining_ms, "max_output_bytes": limits.max_output_bytes, "max_frame_bytes": limits.max_frame_bytes,
        }))?,
    )?;
    prepare_generated_metadata(rootfs, &[GUEST_RUNTIME, GUEST_HOME, "/workspace", "/tmp"])
}
