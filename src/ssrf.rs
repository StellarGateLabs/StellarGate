//! Guards outbound webhook requests against SSRF.
//!
//! `webhook_url` is merchant-supplied and reachable by an unauthenticated caller
//! via the redeliver endpoint, so before every dispatch we resolve the target
//! host ourselves and reject loopback / link-local / private / other reserved
//! ranges. We then pin the connection to the exact address we just validated
//! (via reqwest's per-host `resolve()`) so a DNS answer that changes between
//! our check and the actual connect — a DNS-rebinding attack — can't slip a
//! blocked address past us: the pinned client never re-resolves the host.

use anyhow::{Result, anyhow};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// Bounds how long a webhook host lookup may take, so a slow or black-holed
/// DNS server can't stall a request indefinitely.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

/// A webhook target resolved to one concrete address and confirmed safe (or,
/// with `allow_private` set, resolved but not range-checked — dev/test only).
#[derive(Debug)]
pub struct SafeTarget {
    host: String,
    addr: SocketAddr,
}

impl SafeTarget {
    /// The host the URL names, as resolved and validated. The client built by
    /// [`pinned_client`] is pinned to [`addr`](Self::addr) for *this* host, so a
    /// cached client must never be reused for a different host that happens to
    /// resolve to the same address.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The one concrete address `validate` confirmed safe (or, with
    /// `allow_private`, resolved without the range check). This is the address
    /// the pinned client connects to regardless of later DNS answers.
    pub fn addr(&self) -> SocketAddr {
        self.addr
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

    let addr = tokio::time::timeout(
        RESOLVE_TIMEOUT,
        tokio::net::lookup_host((host.as_str(), port)),
    )
    .await
    .map_err(|_| anyhow!("timed out resolving webhook host {host:?}"))?
    .map_err(|e| anyhow!("failed to resolve webhook host {host:?}: {e}"))?
    .next()
    .ok_or_else(|| anyhow!("webhook host {host:?} did not resolve to any address"))?;

    if !allow_private && is_blocked_ip(addr.ip()) {
        return Err(anyhow!(
            "webhook_url host {host:?} resolves to a disallowed address ({})",
            addr.ip()
        ));
    }

    Ok(SafeTarget { host, addr })
}

/// Build a client that connects to `target.addr` for `target.host` regardless
/// of what a later DNS lookup for that host would return, so the connection
/// actually made is the one already validated by `validate`.
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
        .resolve(&target.host, target.addr)
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
