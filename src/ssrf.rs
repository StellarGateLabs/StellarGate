//! Guards outbound webhook requests against SSRF.
//!
//! `webhook_url` is merchant-supplied and reachable by an unauthenticated caller
//! via the redeliver endpoint, so before every dispatch we resolve the target
//! host ourselves and reject loopback / link-local / private / other reserved
//! ranges. We then pin the connection to the exact address we just validated
//! (via reqwest's per-host `resolve()`) so a DNS answer that changes between
//! our check and the actual connect — a DNS-rebinding attack — can't slip a
//! blocked address past us: the pinned client never re-resolves the host.

use anyhow::{anyhow, Result};
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
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (ip.segments()[0] & 0xfe00) == 0xfc00 // unique local, RFC 4193
        || (ip.segments()[0] & 0xffc0) == 0xfe80 // link-local unicast, RFC 4291
        || ip.to_ipv4_mapped().is_some_and(is_blocked_ipv4)
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
                is_blocked_ipv4(ip.parse().unwrap()),
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
                is_blocked_ipv6(ip.parse().unwrap()),
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
}
