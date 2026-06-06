//! `pillbox vault …` handlers — inspect the MITM credential proxy's
//! CA cert + on-disk state. The actual vault server runs via
//! `pillbox sidecar`; these subcommands are introspection only.

use anyhow::Result;

use crate::cli::VaultAction;
use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::{paths, vault};

pub(crate) fn dispatch(resolved: &Pillbox, action: VaultAction) -> Result<()> {
    match action {
        VaultAction::Ca { json } => ca(resolved, json),
        VaultAction::Status { json } => status(resolved, json),
    }
}

fn ca(resolved: &Pillbox, json: bool) -> Result<()> {
    let ca_dir = resolved.subdir("vault")?;
    let ca = vault::Ca::ensure(&ca_dir)
        .map_err(|e| PillboxError::runtime("vault ca", format!("ensure ca: {e}")))?;
    if json {
        println!(
            "{}",
            paths::json_v1(vec![(
                "ca_cert_path",
                serde_json::Value::String(ca.cert_path().display().to_string()),
            )]),
        );
    } else {
        println!("{}", ca.cert_path().display());
        eprintln!(
            "pillbox: pinned a stable vault CA — `--vault` runs will now reuse it \
             instead of minting a per-run ephemeral one."
        );
    }
    Ok(())
}

fn status(resolved: &Pillbox, json: bool) -> Result<()> {
    let ca_dir = resolved.subdir("vault")?;
    let ca_cert = vault::ca_cert_path_in(&ca_dir);
    let exists = ca_cert.exists();
    if json {
        let cert_path_val = if exists {
            serde_json::Value::String(ca_cert.display().to_string())
        } else {
            serde_json::Value::Null
        };
        println!(
            "{}",
            paths::json_v1(vec![
                ("ca_exists", serde_json::Value::Bool(exists)),
                // Which trust root `--vault` runs use: a persisted stable CA if
                // present, else a fresh per-run ephemeral one.
                (
                    "ca_mode",
                    serde_json::Value::String(
                        if exists { "stable" } else { "per-run" }.to_string()
                    )
                ),
                (
                    "ca_dir",
                    serde_json::Value::String(ca_dir.display().to_string())
                ),
                ("ca_cert_path", cert_path_val),
                (
                    "pillbox",
                    serde_json::Value::String(resolved.display_name().into())
                ),
            ]),
        );
        return Ok(());
    }
    if exists {
        println!(
            "Stable vault CA for `{}` at {}",
            resolved.display_name(),
            ca_cert.display()
        );
        println!("`--vault` runs reuse it. (Delete it to switch to per-run ephemeral CAs.)");
    } else {
        println!(
            "`--vault` runs for `{}` use a per-run ephemeral CA (the default).",
            resolved.display_name()
        );
        println!();
        println!("A fresh CA is minted per run and discarded after — a leaked CA is");
        println!("valid only for that run. To pin a stable CA instead (e.g. to");
        println!("pre-trust it in a browser for debugging), run `pillbox vault ca`.");
    }
    Ok(())
}
