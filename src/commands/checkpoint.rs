//! `pillbox checkpoint pack|unpack` — the session packet engine subcommands
//! Stratus calls. Flags, output and the packet itself live in the
//! `pillbox-packet` crate (`crates/packet/`); this only names the program and
//! maps the crate's error onto pillbox's exit categories.

use anyhow::Result;
use pillbox_packet::{Names, PacketError};

pub(crate) use pillbox_packet::Command as CheckpointAction;

pub(crate) fn dispatch(action: CheckpointAction) -> Result<()> {
    let names = Names {
        program: "pillbox",
        command: "checkpoint ",
    };
    pillbox_packet::run(action, &names).map_err(|err| match err.downcast::<PacketError>() {
        Ok(e) => crate::errors::PillboxError::from(e).into(),
        Err(err) => err,
    })
}
