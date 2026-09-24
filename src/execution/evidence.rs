//! Durable runtime evidence in the existing session log and content-addressed store.
//! Native projections are observations only; runtime terminal decisions belong to the caller.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::contract::{Actor, Artifact, ArtifactClass, Custom, Event, Payload};
use crate::events::{blob::BlobStore, codex_serve::CodexServeMapper, log::SessionLog};
use crate::pillbox::Pillbox;

use super::files::FileTree;
use super::{digest, identity, protocol, ArtifactRef, EvidenceRef, MAX_EVIDENCE_BYTES};

const MAX_NATIVE_FRAMES: usize = 16_384;
const MAX_LOG_EVENT_BYTES: usize = 8 * 1024 * 1024;
const ADMISSION_EVENT: &str = "repository.execution.admitted";

pub(crate) struct ExecutionEvidence {
    session_id: String,
    run_id: String,
    dir: PathBuf,
    log: SessionLog,
    blobs: BlobStore,
    last_seq: u64,
    native_written: bool,
}

impl ExecutionEvidence {
    pub(crate) fn start(
        pb: &Pillbox,
        session_id: &str,
        run_id: &str,
        admission: Value,
    ) -> Result<Self> {
        identity(session_id)?;
        identity(run_id)?;
        let path = crate::session::session_dir_path(pb, session_id);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                metadata.is_dir(),
                "execution session directory is not a regular directory"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect execution session directory"),
        }
        let dir = crate::session::session_dir(pb, session_id)?;
        regular_directory(&dir)?;
        match fs::symlink_metadata(dir.join("log.jsonl")) {
            Ok(metadata) => ensure!(
                metadata.is_file(),
                "execution session log is not a regular file"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect execution session log"),
        }
        let mut log = SessionLog::open_at(dir.clone())?;
        let event = Event::session(
            session_id,
            Payload::Custom(Custom {
                name: ADMISSION_EVENT.into(),
                payload: Some(admission),
            }),
        )
        .with_run(run_id)
        .with_actor(runtime_actor());
        let last_seq = log.append_exact_batch(&[event], None, |pre_seq| {
            ensure!(
                pre_seq == 0,
                "execution evidence session is already occupied"
            );
            // Sequence recovery tolerates torn records. An occupied file with no
            // recoverable seq is still not a fresh admission session.
            let metadata = fs::symlink_metadata(dir.join("log.jsonl"))
                .context("inspect admission log while locked")?;
            ensure!(
                metadata.is_file()
                    && metadata.len() == 0
                    && metadata.nlink() == 1
                    && metadata.permissions().mode() & 0o077 == 0,
                "execution admission requires an empty private regular session log"
            );
            Ok(())
        })?;
        sync_directory(&dir)?;
        sync_directory(dir.parent().context("session directory has no parent")?)?;
        sync_directory(&pb.state_dir)?;
        Ok(Self {
            session_id: session_id.into(),
            run_id: run_id.into(),
            blobs: BlobStore::open_at(dir.clone()),
            dir,
            log,
            last_seq,
            native_written: false,
        })
    }

    pub(crate) fn append(&mut self, payload: Payload) -> Result<EvidenceRef> {
        self.append_as(payload, runtime_actor())
    }

    fn append_as(&mut self, payload: Payload, actor: Actor) -> Result<EvidenceRef> {
        let event = Event::session(&self.session_id, payload)
            .with_run(&self.run_id)
            .with_actor(actor);
        let seq = self.log.append(&[event])?;
        sync_directory(&self.dir)?;
        self.last_seq = seq;
        Ok(EvidenceRef {
            session_id: self.session_id.clone(),
            seq_range: [seq, seq],
        })
    }

    pub(crate) fn reference(&self) -> EvidenceRef {
        EvidenceRef {
            session_id: self.session_id.clone(),
            seq_range: [1, self.last_seq],
        }
    }

    /// Store content only; the caller chooses the semantic log payload that links it.
    pub(crate) fn artifact(&self, bytes: &[u8], media_type: &str) -> Result<ArtifactRef> {
        ensure!(
            bytes.len() as u64 <= MAX_EVIDENCE_BYTES,
            "execution artifact byte limit exceeded"
        );
        ensure!(
            !media_type.is_empty()
                && media_type.len() <= 128
                && media_type.bytes().all(|byte| (b'!'..=b'~').contains(&byte)),
            "invalid execution artifact media type"
        );
        regular_directory(&self.dir)?;
        let expected = digest(bytes);
        let expected_handle = expected.strip_prefix("sha256:").unwrap();
        let path = self.blobs.path(expected_handle)?;
        let parent = path.parent().context("blob path has no parent")?;
        match fs::symlink_metadata(parent) {
            Ok(metadata) => ensure!(
                metadata.is_dir(),
                "execution blob directory is not a regular directory"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect execution blob directory"),
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(metadata.is_file(), "execution blob is not a regular file"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect execution blob"),
        }
        let handle = self.blobs.put(bytes)?;
        ensure!(
            handle == expected_handle,
            "blob store returned a different digest"
        );
        // BlobStore deduplicates by filename. Reopen without following symlinks,
        // verify the bytes, and make both content and its directory entry durable.
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .context("open execution blob for verification")?;
        let metadata = file.metadata().context("inspect stored execution blob")?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "stored execution blob is not a private regular file"
        );
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "stored execution blob permissions are not private"
        );
        ensure!(
            metadata.len() == bytes.len() as u64,
            "stored execution blob length mismatch"
        );
        let mut hasher = Sha256::new();
        let mut offset = 0;
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .context("verify stored execution blob bytes")?;
            if count == 0 {
                break;
            }
            ensure!(
                count <= bytes.len() - offset && buffer[..count] == bytes[offset..offset + count],
                "stored execution blob content mismatch"
            );
            hasher.update(&buffer[..count]);
            offset += count;
        }
        ensure!(
            offset == bytes.len() && format!("sha256:{:x}", hasher.finalize()) == expected,
            "stored execution blob digest mismatch"
        );
        file.sync_all().context("fsync verified execution blob")?;
        sync_directory(parent)?;
        sync_directory(&self.dir)?;
        Ok(ArtifactRef {
            session_id: self.session_id.clone(),
            digest: expected,
            bytes: bytes.len() as u64,
            media_type: media_type.into(),
        })
    }

    pub(crate) fn snapshot(&self, tree: &FileTree) -> Result<ArtifactRef> {
        for entry in tree.entries() {
            self.artifact(&entry.bytes, "application/octet-stream")?;
        }
        let bytes = tree.manifest_bytes()?;
        ensure!(
            digest(&bytes) == tree.digest(),
            "execution snapshot manifest differs from FileTree identity"
        );
        self.artifact(&bytes, "application/vnd.pillbox.file-tree+json")
    }

    /// Called once with the complete observed exchange, including on native failure.
    pub(crate) fn native_frames(&mut self, frames: &[Value]) -> Result<ArtifactRef> {
        ensure!(
            !self.native_written,
            "native execution evidence was already recorded"
        );
        ensure!(
            frames.len() <= MAX_NATIVE_FRAMES,
            "native evidence frame limit exceeded"
        );
        let mut output = BoundedBytes::new(MAX_EVIDENCE_BYTES as usize);
        for envelope in frames {
            let object = envelope
                .as_object()
                .context("native evidence envelope is not an object")?;
            ensure!(
                object.len() == 2
                    && object.contains_key("message")
                    && matches!(envelope["direction"].as_str(), Some("inbound" | "outbound")),
                "invalid native evidence envelope"
            );
            serde_json::to_writer(&mut output, envelope)
                .context("native evidence byte limit exceeded")?;
            output
                .write_all(b"\n")
                .context("native evidence byte limit exceeded")?;
        }
        let artifact = self.artifact(&output.bytes, "application/x-ndjson")?;
        // Once any log append begins, replaying this method could duplicate observations.
        self.native_written = true;
        self.append(Payload::Artifact(Artifact {
            kind: "repository.native_rpc".into(),
            summary: String::new(),
            content_type: artifact.media_type.clone(),
            class: ArtifactClass::Content,
            blob_ref: artifact.digest.strip_prefix("sha256:").unwrap().into(),
            bytes: artifact.bytes,
            worker_id: String::new(),
        }))?;
        if let Some((thread_id, turn_id)) = native_identity(frames) {
            let mut mapper = CodexServeMapper::new();
            for (index, envelope) in frames.iter().enumerate() {
                let message = &envelope["message"];
                if envelope["direction"] != "inbound"
                    || message.get("id").is_some()
                    || !correlated_notification(message, &thread_id, &turn_id)
                {
                    continue;
                }
                for payload in mapper.on_notification(message) {
                    if !matches!(
                        payload,
                        Payload::MessageStart(_)
                            | Payload::MessageDelta(_)
                            | Payload::MessageEnd(_)
                            | Payload::ToolCall(_)
                            | Payload::Thinking(_)
                            | Payload::Usage(_)
                    ) {
                        continue;
                    }
                    let event = Event::session(&self.session_id, payload.clone())
                        .with_run(&self.run_id)
                        .with_actor(Actor::agent("codex"));
                    // Reserve space for the authoritative seq replacing zero.
                    let mut counter = ByteCounter {
                        remaining: MAX_LOG_EVENT_BYTES - 20,
                    };
                    if serde_json::to_writer(&mut counter, &event).is_err() {
                        // Raw evidence remains complete when the optional projection
                        // cannot fit the existing session log's smaller record ceiling.
                        self.append(Payload::Custom(Custom {
                            name: "repository.native_projection_omitted".into(),
                            payload: Some(json!({"frameIndex":index,"artifact":artifact,"reason":"session_log_record_limit"})),
                        }))?;
                    } else {
                        self.append_as(payload, Actor::agent("codex"))?;
                    }
                }
            }
        }
        Ok(artifact)
    }
}

fn runtime_actor() -> Actor {
    Actor::service("repository-execution")
}

fn regular_directory(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)
            .context("inspect execution evidence directory")?
            .is_dir(),
        "execution evidence directory is not a regular directory"
    );
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .context("open execution evidence directory for sync")?
        .sync_all()
        .context("fsync execution evidence directory")
}

fn unique_message<'a>(
    frames: &'a [Value],
    direction: &str,
    matches: impl Fn(&Value) -> bool,
) -> Option<&'a Value> {
    let mut matching = frames
        .iter()
        .filter(|frame| frame["direction"] == direction && matches(&frame["message"]));
    let first = &matching.next()?["message"];
    matching.next().is_none().then_some(first)
}

fn native_identity(frames: &[Value]) -> Option<(String, String)> {
    let thread_request = unique_message(frames, "outbound", |msg| msg["method"] == "thread/start")?;
    protocol::unsupported_server_request(thread_request.get("id")?).ok()?;
    let thread_response = unique_message(frames, "inbound", |msg| {
        msg.get("method").is_none()
            && msg.get("error").is_none()
            && msg["id"] == thread_request["id"]
    })?;
    let turn_request = unique_message(frames, "outbound", |msg| msg["method"] == "turn/start")?;
    protocol::unsupported_server_request(turn_request.get("id")?).ok()?;
    let turn_response = unique_message(frames, "inbound", |msg| {
        msg.get("method").is_none() && msg.get("error").is_none() && msg["id"] == turn_request["id"]
    })?;
    let params = &turn_request["params"];
    let profile = protocol::CodexProfile::new_for_version(
        thread_response["result"]["thread"]["cliVersion"].as_str()?,
        params["model"].as_str()?,
        params["effort"].as_str()?,
    )
    .ok()?;
    if thread_request["params"]["model"] != profile.model() {
        return None;
    }
    let thread_id = profile
        .validate_thread_start(&thread_response["result"])
        .ok()?;
    if params["threadId"] != thread_id {
        return None;
    }
    let turn_id = protocol::validate_turn_started(&turn_response["result"]).ok()?;
    Some((thread_id, turn_id))
}

fn correlated_notification(message: &Value, thread_id: &str, turn_id: &str) -> bool {
    let params = &message["params"];
    if params["threadId"] != thread_id {
        return false;
    }
    if message["method"] == "turn/completed" {
        return protocol::validate_turn_terminal(params, thread_id, turn_id).is_ok();
    }
    if params["turnId"] != turn_id {
        return false;
    }
    if message["method"] == "thread/tokenUsage/updated" {
        let last = &params["tokenUsage"]["last"];
        return [
            "inputTokens",
            "cachedInputTokens",
            "outputTokens",
            "reasoningOutputTokens",
            "totalTokens",
        ]
        .iter()
        .all(|key| last[key].as_u64().is_some())
            && last["cachedInputTokens"].as_u64() <= last["inputTokens"].as_u64();
    }
    true
}

struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}

struct ByteCounter {
    remaining: usize,
}

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("byte limit exceeded"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl BoundedBytes {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(io::Error::other("byte limit exceeded"));
        }
        let required = self.bytes.len() + bytes.len();
        if required > self.bytes.capacity() {
            let capacity = required
                .max(self.bytes.capacity().saturating_mul(2))
                .min(self.limit);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::super::files::{FileEntry, FileLimits};
    use crate::pillbox::Scope;

    fn pillbox() -> (tempfile::TempDir, Pillbox) {
        let temp = tempfile::tempdir().unwrap();
        let pb = Pillbox {
            scope: Scope::Global,
            state_dir: temp.path().to_path_buf(),
            meta: None,
        };
        (temp, pb)
    }

    fn start(pb: &Pillbox) -> ExecutionEvidence {
        ExecutionEvidence::start(
            pb,
            "evidence-1",
            "run-1",
            json!({"requestDigest":digest(b"request")}),
        )
        .unwrap()
    }

    fn custom(name: &str) -> Payload {
        Payload::Custom(Custom {
            name: name.into(),
            payload: None,
        })
    }

    fn read_blob(evidence: &ExecutionEvidence, artifact: &ArtifactRef) -> Vec<u8> {
        evidence
            .blobs
            .get(artifact.digest.strip_prefix("sha256:").unwrap())
            .unwrap()
    }

    #[test]
    fn admission_is_first_attributed_event_and_references_use_authoritative_positions() {
        let (_temp, pb) = pillbox();
        let mut evidence = start(&pb);
        assert_eq!(evidence.reference().seq_range, [1, 1]);
        let admission = evidence.log.read_from(0).unwrap().remove(0);
        assert_eq!(admission.actor, Some(runtime_actor()));
        assert_eq!(admission.session_id, "evidence-1");
        assert_eq!(admission.run_id, "run-1");
        assert!(
            matches!(admission.payload, Payload::Custom(Custom { ref name, .. }) if name == ADMISSION_EVENT)
        );
        let mut other_writer = SessionLog::open(&pb, "evidence-1").unwrap();
        assert_eq!(
            other_writer
                .append(&[Event::session("evidence-1", custom("annotation"))])
                .unwrap(),
            2
        );
        assert_eq!(evidence.append(custom("result")).unwrap().seq_range, [3, 3]);
        assert_eq!(evidence.reference().seq_range, [1, 3]);
        assert_eq!(SessionLog::open(&pb, "evidence-1").unwrap().last_seq(), 3);
    }

    #[test]
    fn existing_session_and_torn_history_are_not_fresh_admissions() {
        let (_temp, pb) = pillbox();
        let evidence = start(&pb);
        let before = fs::read(evidence.dir.join("log.jsonl")).unwrap();
        assert!(ExecutionEvidence::start(&pb, "evidence-1", "run-2", json!({})).is_err());
        assert_eq!(fs::read(evidence.dir.join("log.jsonl")).unwrap(), before);
        for (index, bytes) in [b"{\"seq\":".as_slice(), b" \n".as_slice()]
            .iter()
            .enumerate()
        {
            let id = format!("torn-{index}");
            let dir = crate::session::session_dir(&pb, &id).unwrap();
            crate::paths::write_private_file(&dir.join("log.jsonl"), bytes).unwrap();
            assert!(ExecutionEvidence::start(&pb, &id, "run-2", json!({})).is_err());
            assert_eq!(fs::read(dir.join("log.jsonl")).unwrap(), *bytes);
        }
    }

    #[test]
    fn concurrent_admissions_have_exactly_one_winner() {
        let (_temp, pb) = pillbox();
        let barrier = Arc::new(Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|index| {
                let pb = pb.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    ExecutionEvidence::start(
                        &pb,
                        "collision",
                        &format!("run-{index}"),
                        json!({"producer":index}),
                    )
                    .is_ok()
                })
            })
            .collect();
        let successes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(successes, 1);
        assert_eq!(
            SessionLog::open(&pb, "collision")
                .unwrap()
                .read_from(0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn artifact_references_round_trip_private_verified_content_and_deduplicate() {
        let (_temp, pb) = pillbox();
        let evidence = start(&pb);
        for bytes in [
            b"".as_slice(),
            &[0, 255, 7],
            b"structured contents".as_slice(),
        ] {
            let artifact = evidence
                .artifact(bytes, "application/octet-stream")
                .unwrap();
            assert_eq!(artifact.digest, digest(bytes));
            assert_eq!(artifact.bytes, bytes.len() as u64);
            assert_eq!(artifact.session_id, "evidence-1");
            assert_eq!(read_blob(&evidence, &artifact), bytes);
            let path = evidence
                .blobs
                .path(artifact.digest.strip_prefix("sha256:").unwrap())
                .unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                evidence
                    .artifact(bytes, "application/octet-stream")
                    .unwrap()
                    .digest,
                artifact.digest
            );
        }
        assert_eq!(evidence.reference().seq_range, [1, 1]);
    }

    #[test]
    fn corrupt_existing_blobs_fail_without_repairing_or_returning_a_reference() {
        let (_temp, pb) = pillbox();
        let evidence = start(&pb);
        let artifact = evidence.artifact(b"expected", "text/plain").unwrap();
        let path = evidence
            .blobs
            .path(artifact.digest.strip_prefix("sha256:").unwrap())
            .unwrap();
        for corrupt in [b"wrong!!!".as_slice(), b"short".as_slice()] {
            crate::paths::write_private_file(&path, corrupt).unwrap();
            assert!(evidence.artifact(b"expected", "text/plain").is_err());
            assert_eq!(fs::read(&path).unwrap(), corrupt);
        }
    }

    #[test]
    fn unsafe_existing_blob_paths_and_permissions_fail_closed() {
        for case in ["symlink", "hardlink", "mode", "directory"] {
            let (temp, pb) = pillbox();
            let evidence = start(&pb);
            let artifact = evidence.artifact(b"bytes", "text/plain").unwrap();
            let path = evidence
                .blobs
                .path(artifact.digest.strip_prefix("sha256:").unwrap())
                .unwrap();
            match case {
                "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
                "hardlink" => fs::hard_link(&path, temp.path().join("another-link")).unwrap(),
                "directory" => {
                    fs::remove_file(&path).unwrap();
                    fs::create_dir(&path).unwrap();
                }
                _ => {
                    let outside = temp.path().join("outside");
                    crate::paths::write_private_file(&outside, b"bytes").unwrap();
                    fs::remove_file(&path).unwrap();
                    symlink(outside, &path).unwrap();
                }
            }
            assert!(evidence.artifact(b"bytes", "text/plain").is_err(), "{case}");
        }
    }

    #[test]
    fn snapshot_manifest_reconstructs_exact_binary_bytes_modes_and_tree_identity() {
        let (_temp, pb) = pillbox();
        let evidence = start(&pb);
        let limits = FileLimits {
            max_file_bytes: 1024,
            max_snapshot_bytes: 4096,
            max_tool_calls: 1,
            max_output_bytes: 1,
        };
        let tree = FileTree::new(
            vec![
                FileEntry {
                    path: "a".into(),
                    executable: false,
                    bytes: vec![0, 255, 7],
                },
                FileEntry {
                    path: "b/run".into(),
                    executable: true,
                    bytes: vec![0, 255, 7],
                },
                FileEntry {
                    path: "empty".into(),
                    executable: false,
                    bytes: vec![],
                },
            ],
            &limits,
        )
        .unwrap();
        let artifact = evidence.snapshot(&tree).unwrap();
        assert_eq!(artifact.digest, tree.digest());
        let bytes = read_blob(&evidence, &artifact);
        assert_eq!(digest(&bytes), tree.digest());
        let manifest: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(manifest.len(), 3);
        let restored = manifest
            .iter()
            .map(|entry| FileEntry {
                path: entry["path"].as_str().unwrap().into(),
                executable: entry["executable"].as_bool().unwrap(),
                bytes: evidence
                    .blobs
                    .get(
                        entry["sha256"]
                            .as_str()
                            .unwrap()
                            .strip_prefix("sha256:")
                            .unwrap(),
                    )
                    .unwrap(),
            })
            .collect();
        assert_eq!(FileTree::new(restored, &limits).unwrap(), tree);
        assert_eq!(fs::read_dir(evidence.dir.join("blobs")).unwrap().count(), 3);
        assert_eq!(
            evidence
                .snapshot(&FileTree::new(vec![], &limits).unwrap())
                .unwrap()
                .digest,
            digest(b"[]")
        );
    }

    fn envelope(direction: &str, message: Value) -> Value {
        json!({"direction":direction,"message":message})
    }

    fn exchange() -> Vec<Value> {
        let profile = protocol::CodexProfile::new("gpt-5.6-sol", "high").unwrap();
        vec![
            envelope(
                "outbound",
                json!({"id":"thread-rpc","method":"thread/start","params":profile.thread_start(&[]).unwrap()}),
            ),
            envelope(
                "inbound",
                json!({"id":"thread-rpc","result":{
                    "model":"gpt-5.6-sol","modelProvider":protocol::PROVIDER_ID,"reasoningEffort":"high",
                    "approvalPolicy":"never","approvalsReviewer":"user","cwd":protocol::CODEX_CWD,
                    "runtimeWorkspaceRoots":[],"instructionSources":[],"sandbox":{"type":"readOnly","networkAccess":false},
                    "thread":{"id":"native-thread","cliVersion":protocol::CODEX_VERSION,"modelProvider":protocol::PROVIDER_ID,"cwd":protocol::CODEX_CWD,"ephemeral":true}
                }}),
            ),
            envelope(
                "outbound",
                json!({"id":"turn-rpc","method":"turn/start","params":profile.turn_start("native-thread","input").unwrap()}),
            ),
            envelope(
                "inbound",
                json!({"id":"turn-rpc","result":{"turn":{"id":"native-turn","status":"inProgress","items":[],"error":null}}}),
            ),
        ]
    }

    fn notification(method: &str, mut params: Value) -> Value {
        params["threadId"] = json!("native-thread");
        params["turnId"] = json!("native-turn");
        envelope("inbound", json!({"method":method,"params":params}))
    }

    #[test]
    fn native_identity_requires_a_matching_catalog_release_and_model() {
        let mut frames = exchange();
        frames[0]["message"]["params"]["model"] = json!("gpt-6-sol");
        frames[1]["message"]["result"]["model"] = json!("gpt-6-sol");
        frames[1]["message"]["result"]["thread"]["cliVersion"] =
            json!(protocol::GPT6_CODEX_VERSION);
        frames[2]["message"]["params"]["model"] = json!("gpt-6-sol");
        assert_eq!(
            native_identity(&frames),
            Some(("native-thread".into(), "native-turn".into()))
        );
        frames[1]["message"]["result"]["thread"]["cliVersion"] = json!(protocol::CODEX_VERSION);
        assert!(native_identity(&frames).is_none());
        frames[1]["message"]["result"]["thread"]["cliVersion"] =
            json!(protocol::GPT6_CODEX_VERSION);
        frames[0]["message"]["params"]["model"] = json!("gpt-5.6-sol");
        assert!(native_identity(&frames).is_none());
    }

    #[test]
    fn native_frames_preserve_actual_jsonl_and_normalize_only_correlated_observations() {
        let (_temp, pb) = pillbox();
        let mut evidence = start(&pb);
        let mut frames = exchange();
        frames.push(notification(
            "thread/started",
            json!({"thread":{"id":"native-thread"}}),
        ));
        frames.push(notification("item/completed",json!({"item":{"id":"message-1","type":"agentMessage","phase":"final_answer","text":"actual final"}})));
        let mut foreign = notification(
            "item/completed",
            json!({"item":{"id":"message-2","type":"agentMessage","text":"foreign text"}}),
        );
        foreign["message"]["params"]["turnId"] = json!("foreign");
        frames.push(foreign);
        frames.push(notification("thread/tokenUsage/updated",json!({"tokenUsage":{"last":{"inputTokens":9,"cachedInputTokens":2,"outputTokens":3,"reasoningOutputTokens":1,"totalTokens":12}}})));
        frames.push(notification(
            "turn/completed",
            json!({"turn":{"id":"native-turn","status":"completed","items":[],"error":null}}),
        ));
        frames.push(envelope(
            "inbound",
            json!({"method":"unknown","params":{"claim":"success"}}),
        ));
        let artifact = evidence.native_frames(&frames).unwrap();
        let actual: Vec<Value> = String::from_utf8(read_blob(&evidence, &artifact))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(actual, frames);
        let events = evidence.log.read_from(0).unwrap();
        assert!(matches!(
            events[1].payload,
            Payload::Artifact(Artifact {
                class: ArtifactClass::Content,
                ..
            })
        ));
        assert!(events.iter().any(|event| matches!(&event.payload,Payload::MessageDelta(delta) if delta.text=="actual final")));
        assert!(!events.iter().any(|event| matches!(&event.payload,Payload::MessageDelta(delta) if delta.text=="foreign text")));
        assert!(events.iter().any(
            |event| matches!(&event.payload,Payload::Usage(usage) if usage.input_tokens==Some(7))
        ));
        assert!(!events.iter().any(|event| matches!(
            event.payload,
            Payload::RunStarted(_)
                | Payload::RunFinished(_)
                | Payload::RunFailed(_)
                | Payload::AttentionRequired(_)
        )));
        assert!(events[2..]
            .iter()
            .all(|event| event.actor == Some(Actor::agent("codex"))));
        assert!(evidence.native_frames(&frames).is_err());
    }

    #[test]
    fn absent_or_unconfirmed_native_exchange_never_creates_native_success() {
        for frames in [
            vec![],
            vec![notification(
                "turn/completed",
                json!({"turn":{"id":"native-turn","status":"completed","items":[]}}),
            )],
        ] {
            let (_temp, pb) = pillbox();
            let mut evidence = start(&pb);
            evidence.native_frames(&frames).unwrap();
            let events = evidence.log.read_from(0).unwrap();
            assert_eq!(events.len(), 2);
            assert!(matches!(events[1].payload, Payload::Artifact(_)));
        }
    }

    #[test]
    fn large_native_message_keeps_complete_raw_evidence_and_records_projection_omission() {
        let (_temp, pb) = pillbox();
        let mut evidence = start(&pb);
        let mut frames = exchange();
        let text = "x".repeat(MAX_LOG_EVENT_BYTES);
        frames.push(notification(
            "item/completed",
            json!({"item":{
                "type":"agentMessage","id":"large-message","phase":"final_answer","text":text,
            }}),
        ));
        let artifact = evidence.native_frames(&frames).unwrap();
        let raw = String::from_utf8(read_blob(&evidence, &artifact)).unwrap();
        let observed: Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
        assert_eq!(observed["message"]["params"]["item"]["text"], text);
        let events = evidence.log.read_from(0).unwrap();
        assert!(events.iter().any(|event| matches!(&event.payload, Payload::Custom(custom) if custom.name == "repository.native_projection_omitted")));
        assert!(!events
            .iter()
            .any(|event| matches!(event.payload, Payload::MessageDelta(_))));
    }

    #[test]
    fn admission_rejects_public_empty_log_without_writing_private_payload() {
        let (_temp, pb) = pillbox();
        let dir = crate::session::session_dir(&pb, "public-log").unwrap();
        let path = dir.join("log.jsonl");
        crate::paths::write_private_file(&path, b"").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            ExecutionEvidence::start(&pb, "public-log", "run", json!({"private":"value"})).is_err()
        );
        assert!(fs::read(&path).unwrap().is_empty());
    }

    #[test]
    fn native_shape_and_size_limits_fail_before_recording_artifact_events() {
        let (_temp, pb) = pillbox();
        let mut evidence = start(&pb);
        assert!(evidence
            .native_frames(&[json!({"direction":"model","message":{}})])
            .is_err());
        assert!(evidence
            .native_frames(&vec![envelope("inbound", json!({})); MAX_NATIVE_FRAMES + 1])
            .is_err());
        assert_eq!(evidence.reference().seq_range, [1, 1]);
        let mut bounded = BoundedBytes::new(3);
        bounded.write_all(b"abc").unwrap();
        assert!(bounded.write_all(b"d").is_err());
        assert_eq!(bounded.bytes, b"abc");
    }

    #[test]
    fn invalid_identity_or_symlinked_session_never_admits() {
        let (temp, pb) = pillbox();
        assert!(ExecutionEvidence::start(&pb, "../outside", "run", json!({})).is_err());
        let sessions = pb.subdir("sessions").unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, sessions.join("linked")).unwrap();
        assert!(ExecutionEvidence::start(&pb, "linked", "run", json!({})).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }
}
