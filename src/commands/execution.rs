//! Foreground repository execution with separate durable status/cancel commands.

use std::{fs::File, io::Read, path::PathBuf};

use anyhow::{bail, ensure, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::execution::{self, files::FileLimits, snapshot, store::*, ExecuteRequest};
use crate::pillbox::Pillbox;

#[derive(Debug, Subcommand)]
pub(crate) enum ExecutionAction {
    /// Admit and execute one exact request; identical retries return its existing record.
    Execute {
        #[arg(long)]
        request: PathBuf,
        /// Local Git object source. The request binds its exact commit and tree digest.
        #[arg(long)]
        repository: PathBuf,
    },
    /// Read durable invocation state; recover a lost owner as interrupted.
    Status { invocation_id: String },
    /// Record idempotent cancellation intent for the invocation owner.
    Cancel { invocation_id: String },
    /// Compute the immutable regular-file snapshot digest of an exact Git commit.
    Snapshot {
        #[arg(long)]
        repository: PathBuf,
        #[arg(long)]
        commit: String,
    },
}

pub(crate) fn dispatch(pb: &Pillbox, action: ExecutionAction) -> Result<()> {
    if let ExecutionAction::Snapshot { repository, commit } = action {
        let tree = snapshot::read_git_tree(
            &repository,
            &commit,
            &FileLimits {
                max_file_bytes: 8 * 1024 * 1024,
                max_snapshot_bytes: 64 * 1024 * 1024,
                max_tool_calls: 1,
                max_output_bytes: 64 * 1024 * 1024,
            },
        )?;
        println!(
            "{}",
            crate::paths::json_v1(vec![
                ("snapshot_digest", json!(tree.digest())),
                ("files", json!(tree.entries().len())),
            ])
        );
        return Ok(());
    }
    let store = InvocationStore::new(&pb.state_dir.join("repository-executions"))?;
    let record = match action {
        ExecutionAction::Execute {
            request,
            repository,
        } => {
            let mut bytes = Vec::new();
            File::open(&request)
                .with_context(|| format!("open request {}", request.display()))?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 1024 * 1024, "request exceeds 1 MiB");
            let value: Value =
                serde_json::from_slice(&bytes).context("decode execution request")?;
            execute(pb, &store, &repository, value)?
        }
        ExecutionAction::Status { invocation_id } => store.status(&invocation_id)?,
        ExecutionAction::Cancel { invocation_id } => store.cancel(&invocation_id)?,
        ExecutionAction::Snapshot { .. } => unreachable!(),
    };
    println!(
        "{}",
        crate::paths::json_v1(vec![("execution", serde_json::to_value(record)?)])
    );
    Ok(())
}

fn execute(
    pb: &Pillbox,
    store: &InvocationStore,
    repository: &std::path::Path,
    value: Value,
) -> Result<Record> {
    let invocation_id = value
        .get("invocation_id")
        .and_then(Value::as_str)
        .context("execution request requires invocation_id")?;
    // Compare the original before decoding the current closed shape: an unsupported
    // changed retry is still a conflict, never a new admission attempt.
    let canonical = execution::canonical_json(&value)?;
    if let Some(existing) = store.lookup(invocation_id, &canonical)? {
        return existing_record(existing);
    }
    let request: ExecuteRequest = serde_json::from_value(value)?;
    request.validate()?;
    ensure!(
        request.canonical_request()? == canonical,
        "closed request canonicalization changed"
    );
    let backend = crate::sandbox::select_backend();
    ensure!(
        backend.capabilities().repository_execution,
        "selected backend does not support sealed repository execution"
    );
    let mut owner = match store.claim(&request.invocation_id, &canonical)? {
        Claim::Owned(owner) => owner,
        existing => return existing_record(existing),
    };
    match backend.execute_repository(pb, repository, &request, &mut owner) {
        Ok(completion) => {
            // A cancellation accepted before the terminal commit wins the race.
            let status = if owner.cancelled()? {
                Status::Cancelled
            } else {
                Status::Completed
            };
            owner.finish(status, serde_json::to_value(completion)?)?;
        }
        Err(error) => {
            #[cfg(feature = "libkrun")]
            if error
                .downcast_ref::<crate::sandbox::libkrun::repository::TeardownUnconfirmed>()
                .is_some()
            {
                return Err(error);
            }
            let status = if owner.cancelled()? {
                Status::Cancelled
            } else {
                Status::Failed
            };
            owner.finish(
                status,
                json!({"error": format!("{error:#}"), "evidence": owner.record().detail}),
            )?;
        }
    }
    Ok(owner.record().clone())
}

fn existing_record(claim: Claim) -> Result<Record> {
    match claim {
        Claim::Reused(record) => Ok(record),
        Claim::Conflict { existing_request_hash, requested_request_hash } => bail!(
            "invocation conflict: existing {existing_request_hash}, requested {requested_request_hash}"
        ),
        Claim::Owned(_) => bail!("lookup unexpectedly returned execution ownership"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_claim_is_reused_before_new_policy_or_backend_checks() {
        let temp = tempfile::tempdir().unwrap();
        let pb = Pillbox {
            scope: crate::pillbox::Scope::Global,
            state_dir: temp.path().into(),
            meta: None,
        };
        let store = InvocationStore::new(&temp.path().join("claims")).unwrap();
        let value = json!({"invocation_id": "once", "old_policy": true});
        let canonical = execution::canonical_json(&value).unwrap();
        let Claim::Owned(mut owner) = store.claim("once", &canonical).unwrap() else {
            panic!()
        };
        owner
            .finish(Status::Completed, json!({"sampled": 1}))
            .unwrap();
        drop(owner);
        let record = execute(
            &pb,
            &store,
            PathBuf::from("missing-repository").as_path(),
            value,
        )
        .unwrap();
        assert_eq!(record.status, Status::Completed);
        assert_eq!(record.detail, json!({"sampled": 1}));
        let changed = json!({"invocation_id": "once", "unsupported": true});
        let error = execute(&pb, &store, temp.path(), changed).unwrap_err();
        assert!(error.to_string().starts_with("invocation conflict:"));
    }

    #[test]
    fn invalid_new_request_does_not_consume_an_invocation() {
        let temp = tempfile::tempdir().unwrap();
        let pb = Pillbox {
            scope: crate::pillbox::Scope::Global,
            state_dir: temp.path().into(),
            meta: None,
        };
        let store = InvocationStore::new(&temp.path().join("claims")).unwrap();
        assert!(execute(&pb, &store, temp.path(), json!({"invocation_id": "new"})).is_err());
        assert!(!temp.path().join("claims/new").exists());
    }
}
