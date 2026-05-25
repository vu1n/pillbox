//! URL safety predicates shared between sinks that POST event
//! payloads to a configured collector (webhook, OTLP/HTTP). Pillbox
//! warns on plaintext to non-loopback because event payloads carry
//! session ids + user-supplied labels — fine over loopback or an
//! in-cluster collector, almost always a misconfig when sent
//! cleartext across the public internet.

/// Extract the host component from a URL of the form
/// `scheme://[user@]host[:port][/path][?query][#frag]`. String-level
/// only — we're not validating the URL semantically. Returns the
/// borrowed host slice from `url`; `None` when the URL has no
/// `://`, or when the resulting host is empty.
pub(crate) fn host_of(url: &str) -> Option<&str> {
    let (_scheme, rest) = url.split_once("://")?;
    let after_userinfo = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    // IPv6 literals are bracketed (`[::1]`, `[::1]:8080`); the brackets
    // are part of the host and the colon-port split has to ignore the
    // colons inside them. Plain `rsplit_once(':')` would mangle
    // `[::1]` into host=`[::`. Hostnames + IPv4 keep the simple split.
    let host = if after_userinfo.starts_with('[') {
        match after_userinfo.find(']') {
            Some(end) => &after_userinfo[..=end],
            None => after_userinfo,
        }
    } else {
        after_userinfo
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(after_userinfo)
    };
    (!host.is_empty()).then_some(host)
}

/// True for hosts the sandbox should treat as "the host machine":
/// `localhost`, IPv4 `127.0.0.0/8`, IPv6 `::1` / `[::1]`, and
/// `*.localhost`.
///
/// Deliberately *excludes* mDNS `.local` (RFC 6762): link-local
/// names can resolve to routable LAN addresses, so `attacker.local`
/// would silently slip past any loopback-only check.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
        || host.starts_with("127.")
        || host.ends_with(".localhost")
}

/// If `url` is plaintext HTTP to a non-loopback host, return that
/// host slice for the caller's warning message. `None` covers every
/// "no warning needed" case: HTTPS, loopback (see [`is_loopback_host`]),
/// or a non-http(s) URL we won't POST to anyway.
pub(crate) fn plaintext_non_loopback_host(url: &str) -> Option<&str> {
    if !url.starts_with("http://") {
        return None;
    }
    let host = host_of(url)?;
    (!is_loopback_host(host)).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_return_none() {
        for url in [
            "http://localhost/x",
            "http://localhost:4318/v1/logs",
            "http://127.0.0.1:9000",
            "http://127.5.5.5/",
            "http://[::1]:8080/path",
            "http://collector.localhost/y",
            "http://user@127.0.0.1:9999/api",
        ] {
            assert_eq!(plaintext_non_loopback_host(url), None, "url: {url}");
        }
    }

    #[test]
    fn mdns_local_is_not_loopback() {
        // RFC 6762 .local can resolve to a routable LAN address —
        // treating it as loopback would silently let `attacker.local`
        // bypass the plaintext-HTTP warning. See the rationale on
        // `plaintext_non_loopback_host`.
        assert_eq!(
            plaintext_non_loopback_host("http://collector.local/y"),
            Some("collector.local")
        );
    }

    #[test]
    fn https_returns_none_regardless_of_host() {
        // Plaintext-only check — we never warn on https://, even to
        // non-loopback. TLS is the user's "I am okay sending this
        // payload over the wire" signal.
        assert_eq!(
            plaintext_non_loopback_host("https://collector.example.com/v1/logs"),
            None
        );
    }

    #[test]
    fn non_loopback_plaintext_returns_host() {
        assert_eq!(
            plaintext_non_loopback_host("http://collector.example.com:4318/v1/logs"),
            Some("collector.example.com")
        );
        assert_eq!(
            plaintext_non_loopback_host("http://10.0.0.42/events"),
            Some("10.0.0.42")
        );
        // user@ stripped, port stripped, query string ignored.
        assert_eq!(
            plaintext_non_loopback_host("http://user@collector.example.com:8080/api?q=1"),
            Some("collector.example.com")
        );
    }

    #[test]
    fn non_http_url_returns_none() {
        // We don't POST to file:// / gopher:// / ws:// etc. — no
        // warning needed because the caller wouldn't reach here in
        // the first place. The webhook validator rejects them
        // earlier; the OTel sink only ever sees http(s) per the
        // OTLP spec.
        assert_eq!(plaintext_non_loopback_host("file:///etc/passwd"), None);
        assert_eq!(plaintext_non_loopback_host("ws://x/y"), None);
        assert_eq!(plaintext_non_loopback_host(""), None);
    }
}
