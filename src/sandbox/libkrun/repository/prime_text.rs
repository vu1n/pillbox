//! Prime-only guest preparation on the existing invocation-owned VM substrate.
//! There are no workspace shares; real credentials use the owned stdin channel.

use super::*;
use crate::execution::prime_protocol::{ImageProfile, Selection, HOST, PROFILE_PATH};

const RUNTIME: &str = "/opt/pillbox-prime-text-runtime";

pub(crate) fn runner_image_id(
    pb: &crate::pillbox::Pillbox,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<String> {
    let (image, _) = crate::docker::resolve_runner_image(pb);
    let mut command = Command::new("docker");
    command.args(["image", "inspect", &image, "--format", "{{.Id}}"]);
    let output = run_command(
        &mut command,
        deadline,
        cancelled,
        "inspect Prime runner image",
    )?;
    let id = String::from_utf8(output)?.trim().to_owned();
    validate_image_id(&id)?;
    Ok(id)
}

pub(crate) struct PreparedImage {
    runtime: TempDir,
    rootfs: PathBuf,
    pub(crate) profile: ImageProfile,
}

pub(crate) struct CredentialRelease {
    pub(crate) stub: String,
    pub(crate) real: String,
    pub(crate) guest_auth: Vec<u8>,
}

impl PreparedImage {
    pub(crate) fn prepare(
        image: &str,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self> {
        validate_image_id(image)?;
        let (runtime, rootfs) = prepare_rootfs(image, deadline, cancelled)?;
        let path = rootfs.join(PROFILE_PATH.trim_start_matches('/'));
        let mut current = rootfs.clone();
        for component in Path::new(PROFILE_PATH.trim_start_matches('/')).components() {
            current.push(component);
            ensure!(
                !fs::symlink_metadata(&current)?.file_type().is_symlink(),
                "Prime profile has a symlink ancestor"
            );
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= 1024 * 1024,
            "invalid Prime image profile file"
        );
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "Prime image profile exceeds bound"
        );
        let profile = serde_json::from_slice(&bytes).context("read image-owned Prime profile")?;
        Ok(Self {
            runtime,
            rootfs,
            profile,
        })
    }

    pub(crate) fn launch(
        self,
        selection: &Selection,
        release: CredentialRelease,
        limits: VmLimits,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
        stage: &mut dyn FnMut(&'static str),
    ) -> Result<OwnedVm> {
        limits.validate()?;
        ensure!(
            !release.real.is_empty() && !release.stub.is_empty() && release.real != release.stub,
            "invalid Prime credential release"
        );
        ensure!(
            !release
                .guest_auth
                .windows(release.real.len())
                .any(|part| part == release.real.as_bytes()),
            "Prime guest auth contains a real credential"
        );
        stage("image_prepare");
        let ca_dir = self.runtime.path().join("ca");
        fs::create_dir(&ca_dir)?;
        crate::paths::ensure_mode_0700(&ca_dir)?;
        let ca = crate::vault::Ca::ensure(&ca_dir).map_err(anyhow::Error::msg)?;
        fresh_directory(&self.rootfs, GUEST_HOME)?;
        fresh_directory(&self.rootfs, "/workspace")?;
        fresh_directory(&self.rootfs, RUNTIME)?;
        let runtime = self.rootfs.join(RUNTIME.trim_start_matches('/'));
        write_private_file(
            &runtime.join("bridge.py"),
            include_bytes!("prime_bridge.py"),
        )?;
        write_private_file(&runtime.join("ca.crt"), &fs::read(ca.cert_path())?)?;
        let auth = self.rootfs.join("home/pillbox/.prime/agent");
        fs::create_dir_all(&auth)?;
        crate::paths::ensure_mode_0700(&auth)?;
        write_private_file(&auth.join("auth.json"), &release.guest_auth)?;
        write_private_file(
            &auth.join("settings.json"),
            crate::vault::providers::prime::ISOLATED_SETTINGS.as_bytes(),
        )?;
        write_private_file(
            &runtime.join("config.json"),
            &serde_json::to_vec(&serde_json::json!({
                "argv": selection.argv(), "harness_version": selection.harness_version,
                "duration_ms": remaining_ms(deadline)?, "frame_bytes": limits.max_frame_bytes,
                "evidence_bytes": limits.max_output_bytes,
            }))?,
        )?;
        prepare_generated_metadata(&self.rootfs, &[GUEST_HOME, "/workspace", RUNTIME])?;
        stage("guest_prepare");
        let mut vm = launch_prepared(
            self.runtime,
            limits,
            deadline,
            cancelled,
            true,
            |owner, rpc, remaining| VmSpec {
                rootfs: self.rootfs.to_string_lossy().into_owned(),
                vcpus: 2,
                ram_mib: 2048,
                shares: vec![],
                exec: vec![
                    "/usr/bin/python3".into(),
                    "-I".into(),
                    "-S".into(),
                    format!("{RUNTIME}/bridge.py"),
                ],
                vsock: Some(VsockAttach {
                    port: RPC_PORT,
                    host_sock: rpc.to_string_lossy().into_owned(),
                    listen: false,
                }),
                egress: Some(EgressSpec {
                    allowlist: vec![HOST.into()],
                    log_path: None,
                    ca_dir: Some(ca_dir.to_string_lossy().into_owned()),
                    local_forward_port: None,
                    refresh: None,
                }),
                ownership: Some(OwnershipSpec {
                    socket: owner.to_string_lossy().into_owned(),
                    remaining_ms: Some(remaining),
                }),
            },
        )?;
        let delivered = (|| -> Result<()> {
            let bytes = serde_json::to_vec(&vec![SwapPair {
                stub: release.stub,
                real: release.real,
                hosts: vec![HOST.into()],
            }])?;
            let mut stdin = vm
                .process
                .child
                .stdin
                .take()
                .context("Prime VMM credential channel absent")?;
            nonblocking(stdin.as_raw_fd())?;
            write_until(&mut stdin, &bytes, deadline, cancelled)?;
            Ok(())
        })();
        if let Err(error) = delivered {
            return Err(cleanup_failure(error, vm.stop_and_reap()));
        }
        stage("vmm_spawn");
        Ok(vm)
    }
}
