//! The packet's own error: which subcommand failed, why, and the exit-code
//! category a caller branches on.

use std::fmt;

/// The subcommand an error came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Pack,
    Unpack,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Pack => "pack",
            Op::Unpack => "unpack",
        }
    }
}

/// Exit-code category. The numbers are a contract callers (Stratus) script
/// against, and equal pillbox's own `Runtime` and `Usage` exit codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    /// A refused packet or tree: digest mismatch, malformed manifest, a limit
    /// passed, a file changed while read. Exit 1.
    Runtime,
    /// The caller passed something unusable: a bad flag value or a non-empty
    /// target. Exit 2.
    Usage,
}

impl Category {
    pub fn exit_code(self) -> u8 {
        match self {
            Category::Runtime => 1,
            Category::Usage => 2,
        }
    }
}

/// A failure `pack` or `unpack` reports deliberately. Any other error (I/O,
/// say) surfaces as a plain `anyhow::Error` and counts as [`Category::Runtime`].
#[derive(Debug)]
pub struct PacketError {
    pub op: Op,
    pub reason: String,
    pub category: Category,
}

impl PacketError {
    pub(crate) fn refuse(op: Op, reason: impl Into<String>) -> anyhow::Error {
        PacketError {
            op,
            reason: reason.into(),
            category: Category::Runtime,
        }
        .into()
    }

    pub(crate) fn usage(op: Op, reason: impl Into<String>) -> anyhow::Error {
        PacketError {
            op,
            reason: reason.into(),
            category: Category::Usage,
        }
        .into()
    }
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed. {}", self.op.as_str(), self.reason)
    }
}

impl std::error::Error for PacketError {}

/// The exit code for any error `pack`/`unpack`/[`crate::run`] returns.
pub fn exit_code(err: &anyhow::Error) -> u8 {
    err.downcast_ref::<PacketError>()
        .map_or(Category::Runtime, |e| e.category)
        .exit_code()
}
