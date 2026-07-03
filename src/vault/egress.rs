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
// Context: doc://pillbox/vault-egress-default-deny@0001#vault-egress-default-deny

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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

/// SSRF / DNS-rebinding guard for the MITM forward leg: refuse to forward to a
/// real-upstream IP in a private, loopback, link-local, CGNAT, or ULA range.
/// Both legs dial through this: libkrun's `connect_upstream` (the DNS fence
/// already pins names to the gateway, so this closes the complementary hole
/// where an allowlisted *name* resolves host-side to something internal —
/// cloud metadata 169.254.169.254, `10.0.0.0/8`, a LAN box, `::1`) and the
/// docker broker's forward connector (no network fence at all, so this is the
/// only line between a rebind/allowlisted-internal name and an SSRF). Checked at
/// the resolved IP so there's no TOCTOU. Global/public addresses pass.
/// (iron-proxy's guard; see docs/vault.md.)
pub(crate) fn is_denied_egress_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() // 10/8, 172.16/12, 192.168/16
                || v4.is_loopback() // 127/8
                || v4.is_link_local() // 169.254/16 (incl. the 169.254.169.254 metadata endpoint)
                || v4.is_broadcast()
                || v4.is_unspecified() // 0.0.0.0
                || v4.is_documentation()
                // 100.64.0.0/10 — carrier-grade NAT (std's is_shared() is unstable).
                || matches!(v4.octets(), [100, b, ..] if (64..=127).contains(&b))
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() // ::1
                || v6.is_unspecified() // ::
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                // Any embedded IPv4 (::ffff: mapped, 64:ff9b::/96 NAT64, or the
                // deprecated ::a.b.c.d compatible form) — re-check the inner v4 so
                // an internal address can't hide behind a v6 wrapper.
                || embedded_ipv4(v6).is_some_and(|m| is_denied_egress_ip(IpAddr::V4(m)))
        }
    }
}

/// Extract an IPv4 address embedded in a v6 address, for the forms that carry
/// one in their low 32 bits: IPv4-mapped (`::ffff:a.b.c.d`), the NAT64
/// well-known prefix (`64:ff9b::/96`), and the deprecated IPv4-compatible form
/// (`::a.b.c.d`). Returns `None` for a native v6 address. Used only so the SSRF
/// guard can re-check the inner v4 — an internal v4 mustn't slip through wrapped
/// in v6. `::`/`::1` are excluded (handled directly as unspecified/loopback).
/// The RFC 8215 NAT64 *local-use* prefixes (`64:ff9b:1::/48`) aren't covered —
/// non-routable and a niche deployment; the threat model is name-resolves-inward.
fn embedded_ipv4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return Some(mapped);
    }
    let s = v6.segments();
    let low = Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    let nat64 = s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0];
    // v4-compatible ::a.b.c.d: top 96 bits zero. Exclude ::/::1 (low ∈ {0, 1}).
    let v4_compatible = s[0..6] == [0, 0, 0, 0, 0, 0] && !matches!(low.octets(), [0, 0, 0, _]);
    (nat64 || v4_compatible).then_some(low)
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

    #[test]
    fn ssrf_guard_denies_internal_addrs() {
        let denied = [
            "169.254.169.254",          // cloud metadata
            "127.0.0.1",                // loopback
            "10.1.2.3",                 // private
            "172.16.5.5",               // private
            "192.168.1.1",              // private
            "100.64.0.1",               // CGNAT
            "0.0.0.0",                  // unspecified
            "::1",                      // v6 loopback
            "fc00::1",                  // v6 ULA
            "fe80::1",                  // v6 link-local
            "::ffff:10.0.0.1",          // v4-mapped private
            "64:ff9b::169.254.169.254", // NAT64-embedded cloud metadata
            "::169.254.169.254",        // deprecated v4-compatible, internal
            "::ffff:169.254.169.254",   // v4-mapped cloud metadata
        ];
        for s in denied {
            assert!(
                is_denied_egress_ip(s.parse().unwrap()),
                "{s} should be denied"
            );
        }
    }

    #[test]
    fn ssrf_guard_allows_public_addrs() {
        let allowed = [
            "8.8.8.8",
            "1.1.1.1",
            "104.18.0.1",
            "2606:4700::1111",
            "64:ff9b::8.8.8.8", // NAT64 of a *public* v4 must not be over-denied
        ];
        for s in allowed {
            assert!(
                !is_denied_egress_ip(s.parse().unwrap()),
                "{s} should be allowed"
            );
        }
    }
}
