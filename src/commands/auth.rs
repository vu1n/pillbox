//! `pillbox auth …` handlers — agent-provider authentication state
//! (login/list/rm). Auth lives at global scope only in v0.6; the
//! `--global` flag is a no-op surfaced as a hint to the user.

use anyhow::Result;

use crate::cli::AuthAction;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::{agents, paths};

pub(crate) fn dispatch(resolved: &Pillbox, action: AuthAction) -> Result<()> {
    match action {
        AuthAction::Login { agent, global } => {
            note_auth_global_is_implicit(global);
            // Auth is always global today — passing the resolved pillbox
            // keeps the API uniform for the v0.7 per-project override.
            agents::lookup("auth login", &agent)?.login(resolved)
        }
        AuthAction::List { json, global } => {
            note_auth_global_is_implicit(global);
            auth_list(resolved, json)
        }
        AuthAction::Rm { provider, global } => {
            note_auth_global_is_implicit(global);
            auth_rm(resolved, &provider)
        }
    }
}

/// Auth always lives on the global pillbox in v0.6. Surface the implicit
/// behavior on stderr when the user explicitly passes `--global` so they
/// don't silently assume an alternate scope worked. Removed when v0.7
/// adds the per-project override.
fn note_auth_global_is_implicit(passed: bool) {
    if passed {
        eprintln!(
            "pillbox: note: auth always writes to the global pillbox in v0.6; `--global` is implicit."
        );
    }
}

fn auth_list(resolved: &Pillbox, json: bool) -> Result<()> {
    if json {
        println!("{}", build_auth_list_json(resolved));
        return Ok(());
    }
    // Auth currently always lives in global; show that explicitly so the
    // user understands `--global` is implicit.
    let auth_pb = agents::ALL[0].auth_pillbox(resolved);
    println!(
        "Persistent state under `{}` (auth/<provider>/):",
        auth_pb.display_name()
    );
    let mut any = false;
    // Owners only: an alias agent (codex-serve) shares its owner's (codex) auth
    // home, so listing it would duplicate the same credential store row.
    for spec in agents::ALL.iter().filter(|s| s.owns_auth_home()) {
        let home = spec.home_dir(resolved)?;
        if spec.is_authenticated(resolved) {
            println!("  {:<10} ✓ ({})", spec.id(), home.display());
            any = true;
        }
    }
    if !any {
        println!("  (none)");
        println!();
        println!("Run `pillbox auth login --agent claude` to authenticate.");
    }
    Ok(())
}

fn build_auth_list_json(resolved: &Pillbox) -> String {
    let arr: Vec<serde_json::Value> = agents::ALL
        .iter()
        .filter(|s| s.owns_auth_home())
        .map(|spec| {
            let home = spec
                .home_dir(resolved)
                .ok()
                .map(|h| serde_json::Value::String(h.display().to_string()))
                .unwrap_or(serde_json::Value::Null);
            let mut o = serde_json::Map::new();
            o.insert("id".into(), serde_json::Value::String(spec.id().into()));
            o.insert("home".into(), home);
            o.insert(
                "authenticated".into(),
                serde_json::Value::Bool(spec.is_authenticated(resolved)),
            );
            serde_json::Value::Object(o)
        })
        .collect();
    paths::json_v1(vec![("agents", serde_json::Value::Array(arr))])
}

fn auth_rm(resolved: &Pillbox, provider: &str) -> Result<()> {
    let spec = agents::ALL
        .iter()
        .copied()
        .find(|s| s.id() == provider)
        .ok_or_else(|| {
            PillboxError::usage("auth rm", format!("unknown provider `{provider}`"))
                .with_next("pillbox auth list  # see what's available")
        })?;
    if spec.forget(resolved)? {
        println!("Removed {provider} state.");
    } else {
        println!("No state stored for {provider}.");
    }
    Ok(())
}
