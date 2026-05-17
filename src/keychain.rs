//! OS keychain wrapper for pillbox credential storage.
//!
//! Uses the `keyring` crate with per-platform native backends:
//!   - macOS Security framework
//!   - Linux libsecret (Secret Service / DBus)
//!   - Windows DPAPI
//!
//! All credentials are stored under service name `pillbox` with the
//! provider id (e.g. `claude`) as the account. The value is the
//! provider's serialized credentials JSON.

use anyhow::{Context, Result};

use crate::agents;

const SERVICE: &str = "pillbox";

pub fn save(provider: &str, payload: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, provider)
        .with_context(|| format!("open keychain entry for {provider}"))?;
    entry
        .set_password(payload)
        .with_context(|| format!("write keychain entry for {provider}"))?;
    Ok(())
}

pub fn load(provider: &str) -> Result<Option<String>> {
    let entry = keyring::Entry::new(SERVICE, provider)
        .with_context(|| format!("open keychain entry for {provider}"))?;
    match entry.get_password() {
        Ok(payload) => Ok(Some(payload)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read keychain entry for {provider}")),
    }
}

pub fn list() -> Result<()> {
    println!("Stored credentials (service `{SERVICE}`):");
    let mut any = false;
    for spec in agents::ALL {
        let provider = spec.id();
        let entry = keyring::Entry::new(SERVICE, provider)?;
        match entry.get_password() {
            Ok(_) => {
                println!("  {provider:<10} ✓ present");
                any = true;
            }
            Err(keyring::Error::NoEntry) => {}
            Err(e) => println!("  {provider:<10} ⚠ error: {e}"),
        }
    }
    if !any {
        println!("  (none)");
        println!();
        println!("Run `pillbox claude login` to authenticate.");
    }
    Ok(())
}

pub fn remove(provider: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, provider)
        .with_context(|| format!("open keychain entry for {provider}"))?;
    match entry.delete_credential() {
        Ok(()) => {
            println!("Removed credentials for {provider}.");
            Ok(())
        }
        Err(keyring::Error::NoEntry) => {
            println!("No credentials stored for {provider}.");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("delete keychain entry for {provider}")),
    }
}
