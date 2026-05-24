//! URL safety predicates shared between sinks that POST event
//! payloads to a configured collector (webhook, OTLP/HTTP). Pillbox
//! warns on plaintext to non-loopback because event payloads carry
//! session ids + user-supplied labels — fine over loopback or an
//! in-cluster collector, almost always a misconfig when sent
//! cleartext across the public internet.

/// If `url` is plaintext HTTP to a non-loopback host, return that
/// host slice for the caller's warning message. `None` covers every
/// "no warning needed" case: HTTPS, loopback (`localhost`, IPv4
/// `127.0.0.0/8`, IPv6 `::1`, `*.localhost`), or a non-http(s) URL
/// we won't POST to anyway.
///
/// Loopback set deliberately *excludes* mDNS `.local` (RFC 6762):
/// link-local names can resolve to routable LAN addresses, so
/// `attacker.local:9000` would silently bypass the warning if we
/// matched on the suffix. Users running inside a container that
/// names sidecars `*.local` can use `localhost` / `127.0.0.1` or
/// accept the (correct) plaintext warning.
///
/// Host extraction is intentionally string-level — we're not parsing
/// the URL semantically, just deciding loopback-or-not. The lifetime
/// keeps the returned host borrowed from `url` so the caller can
/// interpolate it without an alloc.
pub(crate) fn plaintext_non_loopback_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("http://")?;
    let host_port = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host_port
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_port);
    let is_loopback = matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
        || host.starts_with("127.")
        || host.ends_with(".localhost");
    if is_loopback || host.is_empty() {
        None
    } else {
        Some(host)
    }
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
