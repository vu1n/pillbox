//! Egress policy — the vault broker's default-deny + destination-binding
//! decision layer.
//!
//! Today's vault is a stub-swap MITM that only touches *known provider* hosts
//! and lets everything else pass through. That's not an exfiltration guard: a
//! compromised agent can POST your code to `evil.example` and the proxy waves it
//! by. The broker model (see docs/vault.md) makes egress **default-deny**: an
//! outbound request is allowed only if a provider intercepts the host (→ swap
//! the stub for the real credential, bound to that host) or the host is on an
//! explicit allowlist; otherwise it's blocked.
//!
//! This module is the pure decision — `host + provider-match + policy →
//! {Swap, AllowPassthrough, Deny}` — so it's unit-testable without a running
//! proxy. The server wires it into `should_intercept` (intercept to swap or to
//! block) and `handle_request` (deny → 403).
//!
//! Destination-binding: a provider only intercepts its own host(s), so a stub
//! leaving for the wrong host is never swapped (it ships as a worthless stub) —
//! and under default-deny that wrong-host request is blocked outright. The
//! credential is released only on the host it's bound to.

/// What the broker does with an outbound request to a given host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EgressDecision {
    /// A provider intercepts this host — MITM and swap the stub for the real
    /// credential. The only path on which a real secret is ever released.
    Swap,
    /// Allowed to leave unmodified (tunnel, no MITM): either an explicit
    /// allowlist entry, or — in permissive mode — any unmatched host.
    AllowPassthrough,
    /// Blocked: no provider, not allowlisted, default-deny on. The request
    /// never leaves; the agent receives a 403.
    Deny,
}

/// Broker egress policy. **Default is permissive** (no default-deny, empty
/// allowlist) so enabling `--vault` doesn't change egress until the broker is
/// explicitly turned on — preserving the legacy stub-swap-only behavior.
#[derive(Debug, Clone, Default)]
pub(crate) struct EgressPolicy {
    /// When true, a host that is neither intercepted by a provider nor on
    /// `allow_hosts` is **denied**. This is the real security line. When false
    /// (default), unmatched hosts pass through (legacy behavior).
    pub default_deny: bool,
    /// Hosts allowed to egress unmodified (no credential swap) even under
    /// default-deny — e.g. package registries a build needs. Exact match, or a
    /// leading-dot suffix (`.example.com` matches `a.example.com` and the
    /// apex). Invoker-set, so an untrusted workspace can't widen its own egress.
    pub allow_hosts: Vec<String>,
}

impl EgressPolicy {
    /// Decide what to do with `host`. `intercepted` = whether some provider
    /// claims the host (caller does the lookup; kept out of here so the policy
    /// is pure and testable).
    pub(crate) fn decide(&self, host: &str, intercepted: bool) -> EgressDecision {
        if intercepted {
            EgressDecision::Swap
        } else if !self.default_deny || self.allows(host) {
            // Permissive (the default) short-circuits before the allowlist scan.
            EgressDecision::AllowPassthrough
        } else {
            EgressDecision::Deny
        }
    }

    fn allows(&self, host: &str) -> bool {
        self.allow_hosts
            .iter()
            .any(|entry| host_matches(host, entry))
    }
}

/// Host matches an allowlist entry: exact, or a `.suffix` entry matches the apex
/// (`example.com`) and any subdomain (`a.example.com`). Case-insensitive. A
/// bare `suffix` only matches exactly — so `api.anthropic.com` does NOT match an
/// allow entry of `anthropic.com` unless it's written `.anthropic.com`.
fn host_matches(host: &str, entry: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    let entry = entry.trim().to_ascii_lowercase();
    match entry.strip_prefix('.') {
        Some(suffix) => host == suffix || host.ends_with(&format!(".{suffix}")),
        None => host == entry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(default_deny: bool, allow: &[&str]) -> EgressPolicy {
        EgressPolicy {
            default_deny,
            allow_hosts: allow.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn intercepted_host_always_swaps_even_under_deny() {
        // A provider host swaps regardless of default-deny / allowlist.
        assert_eq!(
            policy(true, &[]).decide("api.anthropic.com", true),
            EgressDecision::Swap
        );
        assert_eq!(
            policy(false, &[]).decide("api.anthropic.com", true),
            EgressDecision::Swap
        );
    }

    #[test]
    fn permissive_passes_unmatched_through() {
        // Legacy behavior: default-deny off → unmatched host leaves unmodified.
        assert_eq!(
            policy(false, &[]).decide("evil.example", false),
            EgressDecision::AllowPassthrough
        );
    }

    #[test]
    fn default_deny_blocks_unmatched() {
        // The real security line: unmatched + not allowlisted → blocked.
        assert_eq!(
            policy(true, &[]).decide("evil.example", false),
            EgressDecision::Deny
        );
    }

    #[test]
    fn allowlist_passes_under_deny_without_swapping() {
        let p = policy(true, &["registry.npmjs.org", ".pythonhosted.org"]);
        assert_eq!(
            p.decide("registry.npmjs.org", false),
            EgressDecision::AllowPassthrough
        );
        // suffix entry matches subdomains + apex
        assert_eq!(
            p.decide("files.pythonhosted.org", false),
            EgressDecision::AllowPassthrough
        );
        assert_eq!(
            p.decide("pythonhosted.org", false),
            EgressDecision::AllowPassthrough
        );
        // but an unrelated host is still denied
        assert_eq!(p.decide("evil.example", false), EgressDecision::Deny);
    }

    #[test]
    fn bare_entry_does_not_match_subdomains() {
        // `anthropic.com` (no leading dot) must not silently allow
        // `api.anthropic.com` — suffix-matching is opt-in via the dot.
        let p = policy(true, &["anthropic.com"]);
        assert_eq!(p.decide("api.anthropic.com", false), EgressDecision::Deny);
        assert_eq!(
            p.decide("anthropic.com", false),
            EgressDecision::AllowPassthrough
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        let p = policy(true, &["Registry.NPMjs.org"]);
        assert_eq!(
            p.decide("registry.npmjs.org", false),
            EgressDecision::AllowPassthrough
        );
    }
}
