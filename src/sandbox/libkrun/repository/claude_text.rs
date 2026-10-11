//! Tool-free Claude launch using the invocation-owned libkrun substrate.

use super::*;
use crate::execution::claude_text::{Credentials, Resolved, PROVIDER_HOST};

pub(crate) struct Input {
    pub(crate) resolved: Resolved,
    pub(crate) prompt: String,
    pub(crate) credentials: Credentials,
    pub(crate) refresh_credentials: PathBuf,
}

// Context: doc://pillbox/adr-001-libkrun-is-the-backend@0001#libkrun-is-the-backend — sealed text runs in an owned microVM.
pub(crate) fn launch(
    input: Input,
    limits: VmLimits,
    cancelled: &dyn Fn() -> bool,
    stage: &mut dyn FnMut(&'static str),
) -> Result<OwnedVm> {
    limits.validate()?;
    validate_image_id(&input.resolved.runner_image_id)?;
    ensure!(
        input.refresh_credentials.is_absolute(),
        "credentials path must be absolute"
    );
    let deadline = Instant::now()
        .checked_add(limits.max_duration)
        .context("VM deadline overflow")?;
    let (runtime, rootfs) = prepare_rootfs(&input.resolved.runner_image_id, deadline, cancelled)?;
    stage("image_prepare");
    let ca_dir = runtime.path().join("ca");
    fs::create_dir(&ca_dir)?;
    crate::paths::ensure_mode_0700(&ca_dir)?;
    let ca = crate::vault::Ca::ensure(&ca_dir).map_err(anyhow::Error::msg)?;
    prepare_guest(
        &rootfs,
        &input,
        &fs::read(ca.cert_path())?,
        limits,
        remaining_ms(deadline)?,
    )?;
    stage("guest_prepare");
    let mut vm = launch_prepared(
        runtime,
        limits,
        deadline,
        cancelled,
        true,
        |owner, rpc, remaining| spec(&rootfs, &ca_dir, owner, rpc, &input, remaining),
    )?;
    let delivery = (|| -> Result<()> {
        let swaps = vec![SwapPair {
            stub: input.credentials.stub,
            real: input.credentials.real,
            hosts: vec![PROVIDER_HOST.into()],
        }];
        let bytes = serde_json::to_vec(&swaps)?;
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
        return Err(cleanup_failure(error, vm.stop_and_reap()));
    }
    stage("vmm_spawn");
    Ok(vm)
}

// Context: doc://pillbox/vault-egress-default-deny@0002#vault-egress-default-deny — only provider egress, no workspace or auth shares.
fn spec(
    rootfs: &Path,
    ca: &Path,
    owner: &Path,
    rpc: &Path,
    input: &Input,
    remaining: u64,
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
            allowlist: vec![PROVIDER_HOST.into()],
            log_path: None,
            ca_dir: Some(ca.to_string_lossy().into_owned()),
            local_forward_port: None,
            refresh: Some(RefreshSpec {
                creds_path: input.refresh_credentials.to_string_lossy().into_owned(),
                auth_id: "claude".into(),
                access_stub: input.credentials.stub.clone(),
            }),
        }),
        ownership: Some(OwnershipSpec {
            socket: owner.to_string_lossy().into_owned(),
            remaining_ms: Some(remaining),
        }),
    }
}

// Context: doc://pillbox/adr-004-vault-broker-oauth@0001#vault-broker-oauth — fresh guest gets only stubs; host owns rotation.
fn prepare_guest(
    rootfs: &Path,
    input: &Input,
    ca: &[u8],
    limits: VmLimits,
    remaining: u64,
) -> Result<()> {
    for path in [
        GUEST_HOME,
        GUEST_RUNTIME,
        "/workspace",
        "/root",
        "/etc/claude-code",
    ] {
        fresh_directory(rootfs, path)?;
    }
    let home = rootfs.join("home/pillbox");
    let auth_dir = home.join(".claude");
    fs::create_dir(&auth_dir)?;
    crate::paths::ensure_mode_0700(&auth_dir)?;
    write_private_file(
        &auth_dir.join(".credentials.json"),
        &input.credentials.guest_auth,
    )?;
    write_private_file(
        &home.join(".claude.json"),
        br#"{"hasCompletedOnboarding":true}"#,
    )?;
    let runtime = rootfs.join(GUEST_RUNTIME.trim_start_matches('/'));
    write_private_file(
        &runtime.join("bridge.py"),
        include_bytes!("claude_text_bridge.py"),
    )?;
    write_private_file(&runtime.join("ca.crt"), ca)?;
    write_private_file(&runtime.join("input.txt"), input.prompt.as_bytes())?;
    write_private_file(
        &runtime.join("launch.json"),
        &serde_json::to_vec(&serde_json::json!({
            "argv": input.resolved.argv(), "duration_ms":remaining,
            "max_output_bytes":limits.max_output_bytes, "max_frame_bytes":limits.max_frame_bytes,
        }))?,
    )?;
    prepare_generated_metadata(
        rootfs,
        &[
            GUEST_RUNTIME,
            GUEST_HOME,
            "/workspace",
            "/root",
            "/etc/claude-code",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_vm_has_only_vault_provider_egress_and_no_host_shares() {
        let input = Input {
            resolved: Resolved::new(
                "claude-opus-4-8",
                "low",
                format!("sha256:{}", "a".repeat(64)),
            )
            .unwrap(),
            prompt: "hello".into(),
            credentials: Credentials {
                guest_auth: b"stub".to_vec(),
                stub: "stub".into(),
                real: "host-only".into(),
            },
            refresh_credentials: PathBuf::from("/private/claude.json"),
        };
        let spec = spec(
            Path::new("/rootfs"),
            Path::new("/ca"),
            Path::new("/owner"),
            Path::new("/rpc"),
            &input,
            100,
        );
        assert!(spec.shares.is_empty());
        let egress = spec.egress.unwrap();
        assert_eq!(egress.allowlist, [PROVIDER_HOST]);
        assert!(egress.local_forward_port.is_none());
        assert_eq!(egress.refresh.unwrap().auth_id, "claude");
    }
}
