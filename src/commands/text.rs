//! Foreground local text execution with durable exact-request recovery.

use std::{fs::File, io::Read, path::PathBuf};

use anyhow::{bail, ensure, Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::execution::{self, store::*};
use crate::pillbox::Pillbox;

#[derive(Debug, Subcommand)]
pub(crate) enum TextAction {
    /// Admit and execute one exact, tool-free text request.
    Execute {
        #[arg(long)]
        request: PathBuf,
    },
    /// Read the original invocation, recovering a lost owner as interrupted.
    Status { invocation_id: String },
    /// Record cancellation intent for the original invocation.
    Cancel { invocation_id: String },
}

pub(crate) fn dispatch(pb: &Pillbox, action: TextAction) -> Result<()> {
    let store = InvocationStore::new(&pb.state_dir.join("text-executions"))?;
    let record = match action {
        TextAction::Execute { request } => {
            let mut bytes = Vec::new();
            File::open(&request)
                .with_context(|| format!("open text request {}", request.display()))?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 1024 * 1024, "text request exceeds 1 MiB");
            let value: Value = serde_json::from_slice(&bytes).context("decode text request")?;
            execute(pb, &store, value)?
        }
        TextAction::Status { invocation_id } => store.status(&invocation_id)?,
        TextAction::Cancel { invocation_id } => store.cancel(&invocation_id)?,
    };
    println!(
        "{}",
        crate::paths::json_v1(vec![("execution", serde_json::to_value(record)?)])
    );
    Ok(())
}

fn execute(pb: &Pillbox, store: &InvocationStore, value: Value) -> Result<Record> {
    let invocation_id = value
        .get("invocation_id")
        .and_then(Value::as_str)
        .context("text request requires invocation_id")?;
    let canonical = execution::canonical_json(&value)?;
    if let Some(existing) = store.lookup(invocation_id, &canonical)? {
        return existing_record(existing);
    }
    let request: execution::text::TextRequest = serde_json::from_value(value)?;
    request.validate()?;
    ensure!(
        execution::canonical_json(&serde_json::to_value(&request)?)? == canonical,
        "closed text request canonicalization changed"
    );
    // This entrypoint exists only in the libkrun build. Claim before any credential
    // read, image preparation or native sampling.
    let mut owner = match store.claim(&request.invocation_id, &canonical)? {
        Claim::Owned(owner) => owner,
        other => return existing_record(other),
    };
    let outcome = execution::text::execute(pb, &request, &mut owner);
    settle(&mut owner, outcome)?;
    Ok(owner.record().clone())
}

pub(crate) fn settle(
    owner: &mut OwnedInvocation,
    result: Result<execution::text::TextCompletion>,
) -> Result<()> {
    match result {
        Ok(completion) => owner.finish(Status::Completed, serde_json::to_value(completion)?)?,
        Err(error) => {
            if error
                .downcast_ref::<crate::sandbox::libkrun::repository::TeardownUnconfirmed>()
                .is_some()
            {
                return Err(error);
            }
            // Full diagnostics live in the restricted Pillbox evidence lane.
            owner.finish(
                Status::Failed,
                json!({"error":"runtime_failed","evidence":owner.record().detail}),
            )?;
        }
    }
    Ok(())
}

fn existing_record(claim: Claim) -> Result<Record> {
    match claim {
        Claim::Reused(record) => Ok(record),
        Claim::Conflict { existing_request_hash, requested_request_hash } => bail!(
            "text invocation conflict: existing {existing_request_hash}, requested {requested_request_hash}"
        ),
        Claim::Owned(_) => bail!("lookup unexpectedly returned text ownership"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_claim_reuses_exact_request_and_interrupts_lost_owner() {
        let temp = tempfile::tempdir().unwrap();
        let store = InvocationStore::new(&temp.path().join("text-executions")).unwrap();
        let canonical =
            execution::canonical_json(&json!({"invocation_id":"once","sealed":true})).unwrap();
        let Claim::Owned(owner) = store.claim("once", &canonical).unwrap() else {
            panic!()
        };
        drop(owner);
        let record = existing_record(store.lookup("once", &canonical).unwrap().unwrap()).unwrap();
        assert_eq!(record.status, Status::Interrupted);
        assert_eq!(store.status("once").unwrap().status, Status::Interrupted);
        let changed =
            execution::canonical_json(&json!({"invocation_id":"once","sealed":false})).unwrap();
        assert!(matches!(
            store.lookup("once", &changed).unwrap(),
            Some(Claim::Conflict { .. })
        ));
    }
}
