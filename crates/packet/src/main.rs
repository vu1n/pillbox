//! `pillbox-packet pack|unpack`: the session packet as a standalone binary.
//! Same flags, JSON and exit codes as `pillbox checkpoint pack|unpack`.

use std::process::ExitCode;

use clap::Parser;
use pillbox_packet::{exit_code, run, Command, Names, PacketError};

/// Write and read pillbox session packets (`pillbox.session-packet/1`).
#[derive(Parser)]
#[command(name = "pillbox-packet", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

const PROGRAM: &str = "pillbox-packet";

fn main() -> ExitCode {
    let cli = Cli::parse();
    let names = Names {
        program: PROGRAM,
        command: "",
    };
    match run(cli.command, &names) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            match err.downcast_ref::<PacketError>() {
                Some(e) => eprintln!("{PROGRAM}: {e}"),
                None => eprintln!("{PROGRAM}: {err:#}"),
            }
            ExitCode::from(exit_code(&err))
        }
    }
}
