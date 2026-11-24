//! Guards outbound webhook requests against SSRF.
//!
//! `webhook_url` is merchant-supplied and reachable by an unauthenticated caller
//! via the redeliver endpoint, so before every dispatch we resolve the target
//! host ourselves and reject loopback / link-local / private / other reserved
//! ranges. Every address the host resolves to must pass, or the URL is
//! rejected. We then pin the connection to exactly that validated set (via
//! reqwest's per-host `resolve_to_addrs()`) so a DNS answer that changes
//! between our check and the actual connect — a DNS-rebinding attack — can't
//! slip a blocked address past us: the pinned client never re-resolves the
//! host. Pinning the whole set, not just the first answer, lets the connector
//! fall back from an unroutable IPv6 address to the host's IPv4 one (#1078).

use anyhow::{Result, anyhow};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// Bounds how long a webhook host lookup may take, so a slow or black-holed
/// DNS server can't stall a request indefinitely.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

/// A webhook target resolved to its addresses and confirmed safe (or, with
/// `allow_private` set, resolved but not range-checked — dev/test only).
#[derive(Debug)]
pub struct SafeTarget {
    host: String,
    /// Non-empty, deduplicated, in resolver order.
    addrs: Vec<SocketAddr>,
}

impl SafeTarget {
    /// The host the URL names, as resolved and validated. The client built by
    /// [`pinned_client`] is pinned to [`addrs`](Self::addrs) for *this* host, so a
    /// cached client must never be reused for a different host that happens to
    /// resolve to the same address.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Every address `validate` confirmed safe (or, with `allow_private`,
    /// resolved without the range check). These are the only addresses the
    /// pinned client connects to, regardless of later DNS answers.
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }
}

/// Validate `url` is an http(s) URL whose host resolves to an address that
/// isn't loopback/link-local/private/reserved. `allow_private` bypasses the
/// range check (still resolves and validates the URL shape) for local
/// development and tests that intentionally target a loopback mock server.
pub async fn validate(url: &str, allow_private: bool) -> Result<SafeTarget> {
    let parsed = reqwest::Url::parse(url).map_err(|e| anyhow!("invalid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(anyhow!("webhook_url must be an http(s) URL, got {other:?}")),
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("webhook_url has no host"))?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("webhook_url has no resolvable port"))?;

    let resolved = tokio::time::timeout(
        RESOLVE_TIMEOUT,
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await
    .map_err(|_| anyhow!("timed out resolving webhook host {host:?}"))?
    .map_err(|e| anyhow!("failed to resolve webhook host {host:?}: {e}"))?;

    let addrs = check_addrs(&host, resolved, allow_private)?;
    Ok(SafeTarget { host, addrs })
}

/// Deduplicate `resolved` and require every address to be allowed: one blocked
/// answer rejects the host, so a record set that mixes a public and a private
/// address cannot be used to reach the private one.
fn check_addrs(
    host: &str,
    resolved: impl IntoIterator<Item = SocketAddr>,
    allow_private: bool,
) -> Result<Vec<SocketAddr>> {
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for addr in resolved {
        if !allow_private && is_blocked_ip(addr.ip()) {
            return Err(anyhow!(
                "webhook_url host {host:?} resolves to a disallowed address ({})",
                addr.ip()
            ));
        }
        if !addrs.contains(&addr) {
            addrs.push(addr);
        }
    }
    if addrs.is_empty() {
        return Err(anyhow!(
            "webhook host {host:?} did not resolve to any address"
        ));
    }
    Ok(addrs)
}

/// Build a client that connects only to `target.addrs` for `target.host`,
/// regardless of what a later DNS lookup for that host would return, so every
/// connection attempt goes to an address already validated by `validate`. The
/// connector tries the addresses in order and falls back to the next one when
/// a connect fails.
///
/// `timeout` is applied per-attempt. Pass `Duration::from_secs(10)` (the
/// default `WEBHOOK_TIMEOUT_SECS`) for webhook delivery; use a longer value
/// for the Horizon general client if needed.
pub fn pinned_client(target: &SafeTarget, timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        // Redirects must be rejected rather than followed: the destination
        // of a redirect has not passed the SSRF validation and pinning above.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("StellarGate/", env!("CARGO_PKG_VERSION")))
        .resolve_to_addrs(&target.host, &target.addrs)
        .build()
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_link_local()
        || ip.is_private()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0 // "this network", RFC 791
        || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT shared space, RFC 6598
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // IETF protocol assignments, RFC 6890
        || (o[0] == 198 && (18..=19).contains(&o[1])) // benchmarking, RFC 2544
        || o[0] >= 240 // reserved + limited broadcast, RFC 1112/6890
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00 // unique local, RFC 4193
        || (seg[0] & 0xffc0) == 0xfe80 // link-local unicast, RFC 4291
        || (seg[0] & 0xffc0) == 0xfec0 // site-local (deprecated), RFC 3879
        || (seg[0] == 0x2001 && seg[1] == 0x0db8) // documentation, RFC 3849
        || (seg[0] == 0x0100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0) // discard-only, RFC 6666
        || ip.to_ipv4_mapped().is_some_and(is_blocked_ipv4)
        || embedded_ipv4(ip).is_some_and(is_blocked_ipv4)
        || is_teredo(ip)
}

/// Extract the IPv4 address embedded in an IPv6 form that routes to or
/// represents an IPv4 host: NAT64 (`64:ff9b::/96`, `64:ff9b:1::/48`), 6to4
/// (`2002::/16`) and IPv4-compatible (`::/96`, deprecated). Returns `None` for
/// addresses that don't embed an IPv4 host.
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = ip.segments();
    // NAT64 well-known prefix 64:ff9b::/96 (RFC 6052 / 8215).
    if seg[0] == 0x0064
        && seg[1] == 0xff9b
        && seg[2] == 0
        && seg[3] == 0
        && seg[4] == 0
        && seg[5] == 0
    {
        return Some(Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        ));
    }
    // NAT64 local-use prefix 64:ff9b:1::/48 (RFC 8215). The embedded IPv4
    // follows the /48 prefix; treat the trailing 32 bits as the IPv4 host.
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2] == 0x0001 {
        return Some(Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        ));
    }
    // 6to4 2002::/16: IPv4 in bits 16–47 (segments 1 and 2).
    if seg[0] == 0x2002 {
        return Some(Ipv4Addr::new(
            (seg[1] >> 8) as u8,
            seg[1] as u8,
            (seg[2] >> 8) as u8,
            seg[2] as u8,
        ));
    }
    // IPv4-compatible ::/96 (deprecated, RFC 4291): first 96 bits zero and not
    // loopback/unspecified (handled above).
    if seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0 {
        return Some(Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            seg[6] as u8,
            (seg[7] >> 8) as u8,
            seg[7] as u8,
        ));
    }
    None
}

/// Teredo (`2001::/32`, RFC 4380) embeds an obfuscated IPv4 address; block it
/// outright rather than trying to decode it.
fn is_teredo(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    seg[0] == 0x2001 && seg[1] == 0x0000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_loopback_link_local_and_private_v4() {
        for ip in [
            "127.0.0.1",
            "169.254.169.254",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
        ] {
            assert!(
                is_blocked_ipv4(ip.parse().expect("private IPv4 fixture should parse")),
                "{ip} should be blocked"
            );
        }
    }

    #[test]
    fn blocks_reserved_and_special_purpose_v4() {
        for ip in [
            "0.0.0.0",
            "100.64.0.1",
            "192.0.0.1",
            "198.18.0.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(
                is_blocked_ipv4(ip.parse().unwrap()),
                "{ip} should be blocked"
            );
        }
    }

    #[test]
    fn allows_public_v4() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            assert!(
                !is_blocked_ipv4(ip.parse().unwrap()),
                "{ip} should be allowed"
            );
        }
    }

    #[test]
    fn blocks_loopback_link_local_and_unique_local_v6() {
        for ip in ["::1", "fe80::1", "fc00::1", "fd00::1"] {
            assert!(
                is_blocked_ipv6(ip.parse().expect("private IPv6 fixture should parse")),
                "{ip} should be blocked"
            );
        }
    }

    #[test]
    fn blocks_ipv4_mapped_private_addresses() {
        assert!(is_blocked_ipv6("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_blocked_ipv6("::ffff:169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn blocks_ipv4_embedding_and_reserved_v6_ranges() {
        // (address, why it must be blocked)
        let cases = [
            ("64:ff9b::7f00:1", "NAT64 -> 127.0.0.1"),
            ("64:ff9b::a9fe:a9fe", "NAT64 -> 169.254.169.254"),
            ("64:ff9b:1::7f00:1", "NAT64 local-use -> 127.0.0.1"),
            ("64:ff9b:1::a9fe:a9fe", "NAT64 local-use -> 169.254.169.254"),
            ("2002:7f00:1::", "6to4 -> 127.0.0.1"),
            ("2002:a9fe:a9fe::", "6to4 -> 169.254.169.254"),
            ("2001::1", "Teredo"),
            ("::7f00:1", "IPv4-compatible -> 127.0.0.1"),
            ("::a9fe:a9fe", "IPv4-compatible -> 169.254.169.254"),
            ("fec0::1", "site-local"),
            ("2001:db8::1", "documentation"),
            ("100::1", "discard-only"),
        ];
        for (ip, why) in cases {
            assert!(
                is_blocked_ipv6(ip.parse().expect("IPv6 fixture should parse")),
                "{ip} ({why}) should be blocked"
            );
        }
    }

    #[test]
    fn allows_public_v6() {
        assert!(!is_blocked_ipv6("2606:4700:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let err = validate("ftp://example.com", false).await.unwrap_err();
        assert!(err.to_string().contains("http(s)"), "got: {err}");
    }

    #[tokio::test]
    async fn rejects_loopback_target() {
        let err = validate("http://127.0.0.1:1234/hook", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("disallowed"), "got: {err}");
    }

    #[tokio::test]
    async fn allows_loopback_when_private_targets_allowed() {
        validate("http://127.0.0.1:1234/hook", true)
            .await
            .expect("loopback must be allowed when allow_private is set");
    }

    // ── #1078: every resolved address is validated and pinned ────────────

    fn sock(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn keeps_every_public_address_in_order_without_duplicates() {
        let v6 = sock("[2606:4700::1111]:443");
        let v4 = sock("1.1.1.1:443");
        let addrs = check_addrs("dual.example", [v6, v4, v6], false).unwrap();
        assert_eq!(addrs, vec![v6, v4]);
    }

    #[test]
    fn rejects_the_host_when_any_resolved_address_is_blocked() {
        for blocked in ["10.0.0.5:443", "[::1]:443", "[fd00::1]:443"] {
            let err = check_addrs("mixed.example", [sock("1.1.1.1:443"), sock(blocked)], false)
                .unwrap_err();
            assert!(err.to_string().contains("disallowed"), "{blocked}: {err}");
        }
    }

    #[test]
    fn rejects_a_host_with_no_addresses() {
        let err = check_addrs("empty.example", [], false).unwrap_err();
        assert!(err.to_string().contains("did not resolve"), "got: {err}");
    }

    #[test]
    fn allow_private_keeps_blocked_addresses() {
        let addrs = check_addrs("dev.example", [sock("127.0.0.1:80")], true).unwrap();
        assert_eq!(addrs, vec![sock("127.0.0.1:80")]);
    }

    /// The first pinned address refuses the connection (nothing listens on
    /// `[::1]` for the mock server), as an IPv6 address does on an IPv4-only
    /// network. The client must fall back to the next validated address.
    #[tokio::test]
    async fn pinned_client_falls_back_to_the_next_validated_address() {
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let port = server.address().port();
        assert!(
            std::net::TcpStream::connect(("::1", port)).is_err(),
            "fixture needs [::1]:{port} to refuse connections"
        );

        let target = SafeTarget {
            host: "receiver.test".into(),
            addrs: vec![
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
                *server.address(),
            ],
        };
        let client = pinned_client(&target, Duration::from_secs(5)).unwrap();
        let res = client
            .post(format!("http://receiver.test:{port}/hook"))
            .send()
            .await
            .expect("must fall back to the IPv4 address");
        assert_eq!(res.status(), 200);
    }

    // ── #441 new tests ────────────────────────────────────────────────────

    #[test]
    fn blocks_broadcast_address_explicitly() {
        // ip.is_broadcast() path: 255.255.255.255
        assert!(
            is_blocked_ipv4("255.255.255.255".parse().unwrap()),
            "broadcast 255.255.255.255 should be blocked"
        );
    }

    #[test]
    fn blocks_documentation_ranges_v4() {
        // RFC 5737 TEST-NET ranges: 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24
        for ip in ["192.0.2.1", "198.51.100.1", "203.0.113.1"] {
            assert!(
                is_blocked_ipv4(ip.parse().unwrap()),
                "{ip} (documentation range) should be blocked"
            );
        }
    }

    #[test]
    fn blocks_multicast_v4() {
        assert!(
            is_blocked_ipv4("224.0.0.1".parse().unwrap()),
            "multicast 224.0.0.1 should be blocked"
        );
    }
}
