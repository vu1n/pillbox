//! The credential files pillbox's agents and vault write, by name. Data only,
//! no dependencies: pillbox's agent registry (`AgentSpec::cred_sentinel`), the
//! vault CA (`src/vault/ca.rs`) and the session packet's exclusions all read
//! these consts, so a credential file is named in exactly one place.

use std::fmt;
use std::path::Path;

/// An agent's credential file, relative to the agent home. Only this crate can
/// make one, and only through the `agent_credentials!` table, which also lists it in
/// [`AGENT_CREDENTIALS`]: every value of this type is in that list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgentCredential(&'static str);

impl AgentCredential {
    /// The path relative to the agent home, e.g. `.codex/auth.json`.
    pub const fn path(self) -> &'static str {
        self.0
    }

    /// The file's own name, e.g. `auth.json` for [`CODEX`].
    pub fn file_name(self) -> &'static str {
        self.0.rsplit('/').next().unwrap_or(self.0)
    }
}

impl AsRef<Path> for AgentCredential {
    fn as_ref(&self) -> &Path {
        Path::new(self.0)
    }
}

impl AsRef<str> for AgentCredential {
    fn as_ref(&self) -> &str {
        self.0
    }
}

impl fmt::Display for AgentCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Defines each agent credential const and [`AGENT_CREDENTIALS`] from one
/// table, so a credential that is not in the list cannot be written.
macro_rules! agent_credentials {
    ($($(#[$doc:meta])* $name:ident = $path:literal;)+) => {
        $($(#[$doc])* pub const $name: AgentCredential = AgentCredential($path);)+
        /// Every agent credential, generated with the consts above.
        pub const AGENT_CREDENTIALS: &[AgentCredential] = &[$($name),+];
    };
}

agent_credentials! {
    /// Claude Code's OAuth credentials.
    CLAUDE = ".claude/.credentials.json";
    /// Codex's credentials (shared by `codex` and `codex-serve`).
    CODEX = ".codex/auth.json";
    /// opencode's SQLite database, where its OAuth tokens live.
    OPENCODE = ".local/share/opencode/opencode.db";
    /// Cursor's credentials.
    CURSOR = ".config/cursor/auth.json";
    /// pi's credentials.
    PI = ".pi/agent/auth.json";
    /// prime-agent's credentials.
    PRIME_AGENT = ".prime/agent/auth.json";
}

/// The vault's self-signed root CA certificate, in the vault directory.
pub const VAULT_CA_CERT: &str = "pillbox-vault-ca.crt";
/// The vault CA's private key, beside [`VAULT_CA_CERT`].
pub const VAULT_CA_KEY: &str = "pillbox-vault-ca.key";

/// Files whose basename the session packet leaves out of every tree it packs,
/// workspace included: Claude's OAuth file and the vault CA.
pub const NEVER_PACKED: &[&str] = &[CLAUDE.path(), VAULT_CA_CERT, VAULT_CA_KEY];
