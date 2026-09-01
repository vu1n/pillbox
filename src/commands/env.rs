//! `pillbox env …` handlers — load/list/show/rm the per-pillbox env
//! bundles that `pillbox run --env BUNDLE` injects into the sandbox.

use anyhow::Result;

use crate::cli::EnvAction;
use crate::envs;
use crate::pillbox::Pillbox;
use crate::WriteScope;

pub(crate) fn dispatch(resolved: &Pillbox, action: EnvAction) -> Result<()> {
    match action {
        EnvAction::Load {
            name,
            path,
            if_not_exists,
            global,
        } => envs::load(
            resolved,
            WriteScope::from_global_flag(global),
            &name,
            &path,
            if_not_exists,
        ),
        EnvAction::List { json } => envs::list(resolved, json),
        EnvAction::Show {
            name,
            reveal,
            to_stdout,
            json,
        } => envs::show(resolved, &name, reveal, to_stdout, json),
        EnvAction::Rm { name, global } => {
            envs::rm(resolved, WriteScope::from_global_flag(global), &name)
        }
    }
}
