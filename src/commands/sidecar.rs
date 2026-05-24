//! `pillbox sidecar` — runs the credential vault as a standalone
//! TCP server (typically a long-lived process orchestrators spawn
//! before invoking the agent). Owns its own tokio runtime; blocks on
//! SIGTERM / SIGINT to shut down cleanly.

use anyhow::Result;

use crate::errors::PillboxError;
use crate::pillbox::Pillbox;
use crate::{paths, vault};

pub(crate) fn run(resolved: &Pillbox, bind: Option<String>, json: bool) -> Result<()> {
    use std::net::SocketAddr;

    let bind_addr =
        match bind {
            Some(s) => Some(s.parse::<SocketAddr>().map_err(|e| {
                PillboxError::usage("sidecar", format!("invalid --bind `{s}`: {e}"))
            })?),
            None => None,
        };

    let ca_dir = resolved.subdir("vault")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| PillboxError::runtime("sidecar", format!("tokio runtime: {e}")))?;

    let server = runtime
        .block_on(vault::Server::start(vault::ServerConfig {
            bind: bind_addr,
            ca_dir,
        }))
        .map_err(|e| PillboxError::runtime("sidecar", format!("start vault server: {e}")))?;

    let listen_addr = server.listen_addr();
    let ca_cert_path = server.ca_cert_path().to_path_buf();
    let pid = std::process::id();

    if json {
        println!(
            "{}",
            paths::json_v1(vec![
                (
                    "listen_addr",
                    serde_json::Value::String(listen_addr.to_string())
                ),
                (
                    "ca_cert_path",
                    serde_json::Value::String(ca_cert_path.display().to_string())
                ),
                ("pid", serde_json::Value::Number(pid.into())),
                (
                    "pillbox",
                    serde_json::Value::String(resolved.display_name().into())
                ),
            ]),
        );
    } else {
        println!(
            "pillbox sidecar listening on {listen_addr} (pillbox: {})",
            resolved.display_name()
        );
        println!("  ca_cert: {}", ca_cert_path.display());
        println!("  pid:     {pid}");
        println!();
        println!("Send SIGTERM (or Ctrl+C) to stop.");
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();

    runtime.block_on(async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| PillboxError::runtime("sidecar", format!("install SIGTERM: {e}")))?;
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .map_err(|e| PillboxError::runtime("sidecar", format!("install SIGINT: {e}")))?;
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        Ok::<(), PillboxError>(())
    })?;

    drop(server);
    Ok(())
}
