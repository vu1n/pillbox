//! Durable ownership for one foreground execution. A recorded claim is never relaunched.

use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Admitted,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl Status {
    pub(crate) fn terminal(self) -> bool {
        !matches!(self, Self::Admitted | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub(crate) invocation_id: String,
    pub(crate) request_hash: String,
    pub(crate) status: Status,
    pub(crate) detail: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Original {
    invocation_id: String,
    request_hash: String,
    canonical_request: String,
}

pub(crate) enum Claim {
    Owned(OwnedInvocation),
    Reused(Record),
    Conflict {
        existing_request_hash: String,
        requested_request_hash: String,
    },
}

pub(crate) struct InvocationStore {
    root: PathBuf,
}

pub(crate) struct OwnedInvocation {
    directory: PathBuf,
    record: Record,
    // Closing this fd releases flock even after a process crash. The durable
    // original outlives the lock and prevents a new owner from sampling again.
    _lock: File,
}

impl InvocationStore {
    pub(crate) fn new(root: &Path) -> Result<Self> {
        private_directory(root)?;
        Ok(Self { root: root.into() })
    }

    /// Check an existing identity before applying current admission policy. A
    /// changed retry conflicts even if its new policy would also be unsupported.
    pub(crate) fn lookup(
        &self,
        invocation_id: &str,
        canonical_request: &str,
    ) -> Result<Option<Claim>> {
        validate_id(invocation_id)?;
        let directory = self.root.join(invocation_id);
        if !directory.exists() {
            return Ok(None);
        }
        let lock = try_lock(&directory)?;
        if !directory.join("original.json").exists() {
            ensure!(
                lock.is_some(),
                "execution admission is being persisted; retry identical request"
            );
            return Ok(None);
        }
        let original = read_original(&directory, invocation_id)?;
        if original.canonical_request != canonical_request {
            return Ok(Some(Claim::Conflict {
                existing_request_hash: original.request_hash,
                requested_request_hash: digest(canonical_request.as_bytes()),
            }));
        }
        Ok(Some(Claim::Reused(recover_record(
            &directory,
            &original,
            lock.is_some(),
        )?)))
    }

    /// Admission must validate the closed wire request before calling this.
    /// `canonical_request` includes every sealed execution and manifest field.
    pub(crate) fn claim(&self, invocation_id: &str, canonical_request: &str) -> Result<Claim> {
        validate_id(invocation_id)?;
        ensure!(
            !canonical_request.is_empty() && canonical_request.len() <= MAX_REQUEST_BYTES,
            "execution request exceeds the bounded claim size"
        );
        let directory = self.root.join(invocation_id);
        private_directory(&directory)?;
        sync_directory(&self.root)?;
        let request_hash = digest(canonical_request.as_bytes());
        let lock = try_lock(&directory)?;
        if directory.join("original.json").exists() {
            let original = read_original(&directory, invocation_id)?;
            if original.canonical_request != canonical_request {
                return Ok(Claim::Conflict {
                    existing_request_hash: original.request_hash,
                    requested_request_hash: request_hash,
                });
            }
            return Ok(Claim::Reused(recover_record(
                &directory,
                &original,
                lock.is_some(),
            )?));
        }
        let Some(lock) = lock else {
            bail!("execution claim is being persisted; retry the identical request");
        };
        // Persist identity before returning the only handle allowed to provision.
        // Failure after this write consumes the invocation; recovery interrupts it.
        let original = Original {
            invocation_id: invocation_id.into(),
            request_hash: request_hash.clone(),
            canonical_request: canonical_request.into(),
        };
        atomic_json(&directory, "original.json", &original)?;
        let record = Record {
            invocation_id: invocation_id.into(),
            request_hash,
            status: Status::Admitted,
            detail: Value::Null,
        };
        atomic_json(&directory, "state.json", &record)?;
        Ok(Claim::Owned(OwnedInvocation {
            directory,
            record,
            _lock: lock,
        }))
    }

    pub(crate) fn status(&self, invocation_id: &str) -> Result<Record> {
        validate_id(invocation_id)?;
        let directory = self.root.join(invocation_id);
        let lock = try_lock(&directory)?;
        let original = read_original(&directory, invocation_id)?;
        recover_record(&directory, &original, lock.is_some())
    }

    pub(crate) fn cancel(&self, invocation_id: &str) -> Result<Record> {
        validate_id(invocation_id)?;
        let directory = self.root.join(invocation_id);
        let _transition = transition_lock(&directory)?;
        let record = self.status(invocation_id)?;
        if !record.status.terminal() {
            // Cancellation is only intent. It cannot overwrite a concurrent
            // terminal result, and the owner must reap execution before sealing.
            atomic_json(&directory, "cancel.json", &true)?;
        }
        Ok(record)
    }
}

impl OwnedInvocation {
    pub(crate) fn record(&self) -> &Record {
        &self.record
    }

    pub(crate) fn cancelled(&self) -> Result<bool> {
        match fs::read(self.directory.join("cancel.json")) {
            Ok(bytes) => {
                ensure!(bytes == b"true", "invalid cancellation intent record");
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("read execution cancellation intent"),
        }
    }

    pub(crate) fn running(&mut self, detail: Value) -> Result<()> {
        ensure!(
            self.record.status == Status::Admitted,
            "execution already started"
        );
        self.write_state(Status::Running, detail)
    }

    pub(crate) fn observe(&mut self, detail: Value) -> Result<()> {
        ensure!(
            self.record.status == Status::Running,
            "execution is not running"
        );
        self.write_state(Status::Running, detail)
    }

    /// Caller must stop/reap the producer and durably capture evidence first.
    pub(crate) fn finish(&mut self, status: Status, detail: Value) -> Result<()> {
        let _transition = transition_lock(&self.directory)?;
        ensure!(
            status.terminal(),
            "a final execution status must be terminal"
        );
        if self.record.status.terminal() {
            ensure!(
                self.record.status == status && self.record.detail == detail,
                "terminal execution evidence is immutable"
            );
            return Ok(());
        }
        // Serialize with cancel(): intent acknowledged before this commit wins;
        // a later cancel sees the immutable terminal record instead.
        let status = if self.cancelled()? {
            Status::Cancelled
        } else {
            status
        };
        self.write_state(status, detail)
    }

    fn write_state(&mut self, status: Status, detail: Value) -> Result<()> {
        let next = Record {
            status,
            detail,
            ..self.record.clone()
        };
        atomic_json(&self.directory, "state.json", &next)?;
        self.record = next;
        Ok(())
    }
}

fn read_original(directory: &Path, invocation_id: &str) -> Result<Original> {
    let original: Original = read_json(&directory.join("original.json"))?;
    ensure!(
        original.invocation_id == invocation_id,
        "claim invocation mismatch"
    );
    ensure!(
        original.canonical_request.len() <= MAX_REQUEST_BYTES
            && original.request_hash == digest(original.canonical_request.as_bytes()),
        "execution claim digest mismatch"
    );
    Ok(original)
}

fn recover_record(directory: &Path, original: &Original, owner_lost: bool) -> Result<Record> {
    let state_path = directory.join("state.json");
    let mut record = if state_path.exists() {
        let record: Record = read_json(&state_path)?;
        ensure!(
            record.invocation_id == original.invocation_id
                && record.request_hash == original.request_hash,
            "execution state does not bind its original claim"
        );
        record
    } else {
        ensure!(owner_lost, "execution admission is still being persisted");
        Record {
            invocation_id: original.invocation_id.clone(),
            request_hash: original.request_hash.clone(),
            status: Status::Admitted,
            detail: Value::Null,
        }
    };
    if owner_lost && !record.status.terminal() {
        record.status = Status::Interrupted;
        record.detail = serde_json::json!({
            "reason": "invocation_owner_lost", "evidence": record.detail,
        });
        atomic_json(directory, "state.json", &record)?;
    }
    Ok(record)
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invocation ID must contain 1..128 ASCII letters, digits, hyphens or underscores"
    );
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create execution directory {}", path.display()))?;
    ensure!(
        fs::symlink_metadata(path)?.file_type().is_dir(),
        "execution state directory is not a directory"
    );
    crate::paths::ensure_mode_0700(path)
}

fn try_lock(directory: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("owner.lock"))
        .context("open execution ownership lock")?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error).context("lock execution ownership")
    }
}

fn transition_lock(directory: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("transition.lock"))
        .context("open execution transition lock")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        ensure!(
            error.kind() == std::io::ErrorKind::WouldBlock,
            "lock execution transition: {error}"
        );
        ensure!(
            std::time::Instant::now() < deadline,
            "execution transition lock timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn atomic_json(directory: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(value).context("serialize execution state")?;
    ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "execution state exceeds its byte limit"
    );
    let temporary = tempfile::NamedTempFile::new_in(directory)
        .context("create execution state staging file")?;
    crate::paths::write_private_file(temporary.path(), &bytes)?;
    temporary
        .as_file()
        .sync_all()
        .context("sync execution state")?;
    temporary
        .persist(directory.join(name))
        .context("commit execution state")?;
    sync_directory(directory)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .with_context(|| format!("sync execution directory {}", path.display()))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    use std::io::Read;
    let mut bytes = Vec::new();
    File::open(path)
        .with_context(|| format!("open execution state {}", path.display()))?
        .take((MAX_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "execution state exceeds its byte limit"
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("decode execution state {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::process::{Command, Stdio};

    fn owned(store: &InvocationStore) -> OwnedInvocation {
        match store.claim("inv-1", r#"{"manifest":"first"}"#).unwrap() {
            Claim::Owned(owner) => owner,
            _ => panic!("expected unique owner"),
        }
    }

    #[test]
    fn duplicate_never_gets_ownership_and_changed_request_conflicts() {
        let root = tempfile::tempdir().unwrap();
        let store = InvocationStore::new(root.path()).unwrap();
        let owner = owned(&store);
        let Claim::Reused(record) = store.claim("inv-1", r#"{"manifest":"first"}"#).unwrap() else {
            panic!("duplicate owned")
        };
        assert_eq!(record.status, Status::Admitted);
        let Claim::Conflict {
            existing_request_hash,
            requested_request_hash,
        } = store.claim("inv-1", r#"{"manifest":"changed"}"#).unwrap()
        else {
            panic!("changed request accepted")
        };
        assert_eq!(existing_request_hash, owner.record().request_hash);
        assert_ne!(existing_request_hash, requested_request_hash);
        assert!(matches!(
            store.lookup("inv-1", "unsupported replacement").unwrap(),
            Some(Claim::Conflict { .. })
        ));
        assert!(store.lookup("not-created", "{}").unwrap().is_none());
    }

    #[test]
    fn owner_loss_is_interrupted_even_before_first_launch() {
        let root = tempfile::tempdir().unwrap();
        let store = InvocationStore::new(root.path()).unwrap();
        drop(owned(&store));
        assert_eq!(store.status("inv-1").unwrap().status, Status::Interrupted);
        let Claim::Reused(record) = store.claim("inv-1", r#"{"manifest":"first"}"#).unwrap() else {
            panic!("reacquired abandoned invocation")
        };
        assert_eq!(record.status, Status::Interrupted);
    }

    #[test]
    fn progress_is_durable_preserved_on_owner_loss_and_immutable_after_finish() {
        let root = tempfile::tempdir().unwrap();
        let store = InvocationStore::new(root.path()).unwrap();
        let mut owner = owned(&store);
        assert!(owner.observe(Value::Null).is_err());
        owner.running(Value::Null).unwrap();
        let progress = serde_json::json!({"verifier_session_id":"verifier-1","builder_evidence":{"seq_range":[1,7]}});
        owner.observe(progress.clone()).unwrap();
        assert_eq!(store.status("inv-1").unwrap().detail, progress);
        drop(owner);
        let interrupted = store.status("inv-1").unwrap();
        assert_eq!(interrupted.status, Status::Interrupted);
        assert_eq!(interrupted.detail["evidence"], progress);
        assert_eq!(store.status("inv-1").unwrap().detail, interrupted.detail);

        let Claim::Owned(mut second) = store.claim("inv-2", "{}").unwrap() else {
            panic!("new invocation not owned")
        };
        second.running(progress.clone()).unwrap();
        second.finish(Status::Failed, progress.clone()).unwrap();
        assert!(second.observe(Value::Null).is_err());
        assert_eq!(store.status("inv-2").unwrap().detail, progress);
    }

    #[test]
    fn cancellation_is_intent_and_terminal_is_immutable() {
        let root = tempfile::tempdir().unwrap();
        let store = InvocationStore::new(root.path()).unwrap();
        let mut owner = owned(&store);
        owner
            .running(serde_json::json!({"session": "builder"}))
            .unwrap();
        assert_eq!(store.cancel("inv-1").unwrap().status, Status::Running);
        assert_eq!(store.cancel("inv-1").unwrap().status, Status::Running);
        assert!(owner.cancelled().unwrap());
        owner.finish(Status::Cancelled, Value::Null).unwrap();
        owner.finish(Status::Cancelled, Value::Null).unwrap();
        assert!(owner.finish(Status::Completed, Value::Null).is_err());
        assert_eq!(store.cancel("inv-1").unwrap().status, Status::Cancelled);
        drop(owner);
        assert_eq!(store.status("inv-1").unwrap().status, Status::Cancelled);
    }

    #[test]
    fn cancellation_and_terminal_commit_have_one_observable_order() {
        for _ in 0..20 {
            let root = tempfile::tempdir().unwrap();
            let store = InvocationStore::new(root.path()).unwrap();
            let mut owner = owned(&store);
            owner.running(Value::Null).unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            std::thread::scope(|scope| {
                let barrier_child = barrier.clone();
                let store_ref = &store;
                let cancel = scope.spawn(move || {
                    barrier_child.wait();
                    store_ref.cancel("inv-1").unwrap()
                });
                barrier.wait();
                owner.finish(Status::Completed, Value::Null).unwrap();
                let observed = cancel.join().unwrap();
                let expected = if observed.status == Status::Running {
                    Status::Cancelled
                } else {
                    Status::Completed
                };
                assert_eq!(owner.record().status, expected);
                assert_eq!(store.status("inv-1").unwrap().status, expected);
            });
        }
    }

    #[test]
    fn torn_admission_consumes_claim_and_corruption_fails_loud() {
        let root = tempfile::tempdir().unwrap();
        let store = InvocationStore::new(root.path()).unwrap();
        drop(owned(&store));
        fs::remove_file(root.path().join("inv-1/state.json")).unwrap();
        assert_eq!(store.status("inv-1").unwrap().status, Status::Interrupted);
        fs::write(root.path().join("inv-1/state.json"), b"{}").unwrap();
        assert!(store.status("inv-1").is_err());
    }

    #[test]
    fn invalid_identity_and_oversize_requests_create_no_claim() {
        let root = tempfile::tempdir().unwrap();
        let store = InvocationStore::new(root.path()).unwrap();
        for id in ["", ".", "..", "a/b", "a\\b", "a:b", "a\n"] {
            assert!(store.claim(id, "{}").is_err());
        }
        assert!(store
            .claim("inv-1", &"x".repeat(MAX_REQUEST_BYTES + 1))
            .is_err());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn child_owner() {
        let Ok(root) = std::env::var("PILLBOX_STORE_TEST_DIRECTORY") else {
            return;
        };
        let store = InvocationStore::new(Path::new(&root)).unwrap();
        let mut owner = owned(&store);
        owner.running(Value::Null).unwrap();
        println!("owner-ready");
        std::io::stdout().flush().unwrap();
        let mut signal = [0];
        std::io::stdin().read_exact(&mut signal).unwrap();
        panic!("test owner must be killed, not resumed");
    }

    #[test]
    fn killed_process_cannot_be_replaced_by_another_sampler() {
        let root = tempfile::tempdir().unwrap();
        let store = InvocationStore::new(root.path()).unwrap();
        let child_test = format!(
            "{}::child_owner",
            module_path!().split_once("::").unwrap().1
        );
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &child_test, "--nocapture"])
            .env("PILLBOX_STORE_TEST_DIRECTORY", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "child exited before admission"
            );
            if line.trim() == "owner-ready" {
                break;
            }
        }
        assert_eq!(store.status("inv-1").unwrap().status, Status::Running);
        assert!(matches!(
            store.claim("inv-1", r#"{"manifest":"first"}"#).unwrap(),
            Claim::Reused(_)
        ));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(store.status("inv-1").unwrap().status, Status::Interrupted);
        assert!(matches!(
            store.claim("inv-1", r#"{"manifest":"first"}"#).unwrap(),
            Claim::Reused(_)
        ));
    }
}
