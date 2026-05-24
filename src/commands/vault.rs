//! `pillbox vault …` handlers — inspect the MITM credential proxy's
//! CA cert + on-disk state. The actual vault server runs via
//! `pillbox sidecar`; these subcommands are introspection only.

use anyhow::Result;

use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::{paths, vault, VaultAction};

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
            "CA for `{}` exists at {}",
            resolved.display_name(),
            ca_cert.display()
        );
        println!();
        println!("Run `pillbox run --vault` to route agent traffic through the proxy.");
    } else {
        println!("No vault CA on disk yet for `{}`.", resolved.display_name());
        println!();
        println!("The CA is created lazily on first `pillbox run --vault`,");
        println!("or eagerly with `pillbox vault ca`.");
    }
    Ok(())
}
