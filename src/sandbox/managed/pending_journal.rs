//! Durable transaction intent for one foreground managed turn.
//!
//! This journal is not an event authority. `Prepared` preserves the exact
//! request identity across a lost response. `Committing` binds one validated
//! payload batch to the authoritative local log sequence observed while holding
//! that log's existing lock; recovery can therefore accept only that exact
//! prefix and append only its missing suffix.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::contract::Payload;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;

use super::execution_contract::{PendingRequest, MAX_EVIDENCE_EVENTS};

const JOURNAL_FILE: &str = "pending-managed-invocation.json";
const JOURNAL_VERSION: u8 = 2;
const MAX_COMMIT_PAYLOADS: usize = MAX_EVIDENCE_EVENTS + 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct CommitIntent {
    pub(super) pre_append_seq: u64,
    pub(super) payloads: Vec<Payload>,
    payload_sha256: String,
    /// `None` is success; `Some` is the already-validated terminal failure to
    /// reproduce after the local evidence commit completes.
    pub(super) terminal_error: Option<String>,
}

impl CommitIntent {
    pub(super) fn new(
        pre_append_seq: u64,
        payloads: Vec<Payload>,
        terminal_error: Option<String>,
    ) -> Result<Self> {
        validate_payload_count(&payloads)?;
        Ok(Self {
            pre_append_seq,
            payload_sha256: payload_sha256(&payloads)?,
            payloads,
            terminal_error,
        })
    }

    fn validate(&self) -> Result<()> {
        validate_payload_count(&self.payloads)?;
        if self.payload_sha256 != payload_sha256(&self.payloads)? {
            return Err(PillboxError::config(
                "session send",
                "pending managed commit payload identity is corrupted or mismatched",
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum PendingState {
    Prepared {
        version: u8,
        request: PendingRequest,
    },
    Committing {
        version: u8,
        request: PendingRequest,
        commit: CommitIntent,
    },
}

impl PendingState {
    pub(super) fn prepared(request: PendingRequest) -> Self {
        Self::Prepared {
            version: JOURNAL_VERSION,
            request,
        }
    }

    pub(super) fn committing(request: PendingRequest, commit: CommitIntent) -> Self {
        Self::Committing {
            version: JOURNAL_VERSION,
            request,
            commit,
        }
    }

    pub(super) fn request(&self) -> &PendingRequest {
        match self {
            Self::Prepared { request, .. } | Self::Committing { request, .. } => request,
        }
    }

    pub(super) fn commit(&self) -> Option<&CommitIntent> {
        match self {
            Self::Prepared { .. } => None,
            Self::Committing { commit, .. } => Some(commit),
        }
    }

    fn validate(&self) -> Result<()> {
        let version = match self {
            Self::Prepared { version, .. } | Self::Committing { version, .. } => *version,
        };
        if version != JOURNAL_VERSION {
            return Err(PillboxError::config(
                "session send",
                format!("unsupported pending managed journal version {version}"),
            )
            .into());
        }
        if let Some(commit) = self.commit() {
            commit.validate()?;
        }
        Ok(())
    }
}

pub(super) struct PendingJournal {
    path: PathBuf,
}

impl PendingJournal {
    pub(super) fn open(resolved: &Pillbox, session_id: &str) -> Result<Self> {
        Ok(Self {
            path: crate::session::session_dir(resolved, session_id)?.join(JOURNAL_FILE),
        })
    }

    pub(super) fn load(&self) -> Result<Option<PendingState>> {
        let body = match std::fs::read(&self.path) {
            Ok(body) => body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.path.display()))
            }
        };
        let state: PendingState = serde_json::from_slice(&body).map_err(|error| {
            PillboxError::config(
                "session send",
                format!(
                    "invalid pending managed invocation {}: {error}",
                    self.path.display()
                ),
            )
        })?;
        state.validate()?;
        Ok(Some(state))
    }

    pub(super) fn persist(&self, state: &PendingState) -> Result<()> {
        state.validate()?;
        let bytes = serde_json::to_vec(state).context("serialize pending managed invocation")?;
        let dir = self
            .path
            .parent()
            .expect("pending journal path has a session directory");
        let mut temp = tempfile::Builder::new()
            .prefix(".pending-managed-")
            .tempfile_in(dir)
            .with_context(|| format!("create pending marker in {}", dir.display()))?;
        temp.as_file_mut()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .context("chmod pending managed invocation 0600")?;
        temp.write_all(&bytes)
            .context("write pending managed invocation")?;
        temp.as_file()
            .sync_all()
            .context("fsync pending managed invocation")?;
        temp.persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("persist {}", self.path.display()))?;
        sync_dir(dir)
    }

    pub(super) fn clear(&self) -> Result<()> {
        std::fs::remove_file(&self.path)
            .with_context(|| format!("clear {}", self.path.display()))?;
        sync_dir(
            self.path
                .parent()
                .expect("pending journal path has a session directory"),
        )
    }

    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("fsync {}", dir.display()))
}

fn validate_payload_count(payloads: &[Payload]) -> Result<()> {
    if payloads.is_empty() || payloads.len() > MAX_COMMIT_PAYLOADS {
        return Err(PillboxError::config(
            "session send",
            "pending managed commit has an invalid payload count",
        )
        .into());
    }
    Ok(())
}

fn payload_sha256(payloads: &[Payload]) -> Result<String> {
    let bytes = serde_json::to_vec(payloads).context("serialize managed commit payloads")?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{Custom, Payload};

    fn request() -> PendingRequest {
        PendingRequest {
            invocation_id: "invocation-1".into(),
            request_body: "{}".into(),
            body_sha256: format!("sha256:{}", "a".repeat(64)),
            request_hash: format!("sha256:{}", "b".repeat(64)),
            execution_digest: format!("sha256:{}", "c".repeat(64)),
            execution_policy_revision: "policy".into(),
        }
    }

    #[test]
    fn journal_round_trips_both_phases_without_bearer_material() {
        crate::test_util::with_isolated_home("managed-pending-journal", || {
            let resolved = crate::pillbox::global();
            let journal = PendingJournal::open(&resolved, "session-1").unwrap();
            journal.persist(&PendingState::prepared(request())).unwrap();
            assert!(journal.load().unwrap().unwrap().commit().is_none());

            let commit = CommitIntent::new(
                7,
                vec![Payload::Custom(Custom {
                    name: "run_cost".into(),
                    payload: Some(serde_json::json!({"version": 1})),
                })],
                None,
            )
            .unwrap();
            journal
                .persist(&PendingState::committing(request(), commit.clone()))
                .unwrap();
            assert_eq!(journal.load().unwrap().unwrap().commit(), Some(&commit));
            let text = std::fs::read_to_string(journal.path()).unwrap();
            assert!(!text.contains("Bearer"));
            assert!(!text.contains("capability-secret"));
            assert_eq!(
                std::fs::metadata(journal.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        });
    }
}
