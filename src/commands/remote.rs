//! `pillbox remote …` handlers — register, list, remove, info the
//! SSH/E2B remotes that `pillbox run --remote NAME` uses.

use anyhow::Result;

use crate::cli::RemoteAction;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::remote;
use crate::WriteScope;

pub(crate) fn dispatch(resolved: &Pillbox, action: RemoteAction) -> Result<()> {
    match action {
        RemoteAction::Add {
            name,
            url,
            url_flag,
            agent,
            if_not_exists,
            global,
        } => {
            // clap's `conflicts_with` already rejects passing both, so
            // here we just pick whichever was given. Missing-both → a
            // pointed usage error rather than the generic "ARGS missing".
            let url = url.or(url_flag).ok_or_else(|| {
                PillboxError::usage(
                    "remote add",
                    "missing SSH URL — pass it positionally: \
                     `pillbox remote add NAME ssh://user@host`",
                )
            })?;
            remote::add(
                resolved,
                WriteScope::from_global_flag(global),
                &name,
                &url,
                agent,
                if_not_exists,
            )
        }
        RemoteAction::List { json } => remote::list(resolved, json),
        RemoteAction::Rm { name, global } => {
            remote::rm(resolved, WriteScope::from_global_flag(global), &name)
        }
        RemoteAction::Info { name, json } => remote::info(resolved, &name, json),
    }
}
