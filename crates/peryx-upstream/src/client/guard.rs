//! Blocks SSRF to non-global addresses while trusting operator-configured hosts. Checks IP literals
//! before requests and resolved addresses at connection time to prevent DNS rebinding.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

use super::UpstreamError;

#[derive(Clone)]
pub struct OutboundGuard {
    trusted: Arc<HashSet<String>>,
    inner: Arc<dyn Resolve>,
}

impl std::fmt::Debug for OutboundGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutboundGuard")
            .field("trusted", &self.trusted)
            .finish_non_exhaustive()
    }
}

impl OutboundGuard {
    /// Trusts the operator-configured `base` host and each `trusted_hosts` entry.
    pub fn new<I, S>(base: &Url, trusted_hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::for_hosts(
            base.host_str()
                .map(str::to_owned)
                .into_iter()
                .chain(trusted_hosts.into_iter().map(|entry| entry.as_ref().to_owned())),
        )
    }

    /// Trusts each entry of `trusted_hosts`, given as a hostname, an IP literal, or a bracketed
    /// IPv6 literal. Every other destination must be globally routable.
    pub fn for_hosts<I, S>(trusted_hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::with_resolver(trusted_hosts, Arc::new(SystemResolver))
    }

    fn with_resolver<I, S>(trusted_hosts: I, inner: Arc<dyn Resolve>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            trusted: Arc::new(
                trusted_hosts
                    .into_iter()
                    .filter_map(|entry| canonical_entry(entry.as_ref()))
                    .collect::<HashSet<_>>(),
            ),
            inner,
        }
    }

    /// Requires HTTP or HTTPS and rejects untrusted, non-global IP literals before sending.
    /// Hostnames remain subject to the connection-time resolver check.
    ///
    /// # Errors
    /// Returns [`UpstreamError::BlockedDestination`] when the scheme or the literal address is not
    /// allowed.
    pub fn check_url(&self, url: &Url) -> Result<(), UpstreamError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(UpstreamError::BlockedDestination {
                reason: format!("scheme {:?} is not http or https", url.scheme()),
            });
        }
        let Some(ip) = literal_ip(url.host().as_ref()) else {
            return Ok(());
        };
        if self.trusted.contains(&ip.to_string()) || is_global_ip(ip) {
            Ok(())
        } else {
            Err(UpstreamError::BlockedDestination {
                reason: format!("{ip} is not a public address"),
            })
        }
    }
}

impl Resolve for OutboundGuard {
    fn resolve(&self, name: Name) -> Resolving {
        let trusted = self.trusted.contains(&name.as_str().to_ascii_lowercase());
        let inner = self.inner.resolve(name);
        Box::pin(async move {
            let kept = retain_allowed(trusted, inner.await?);
            if kept.is_empty() {
                Err(UpstreamError::BlockedDestination {
                    reason: "host resolves only to non-public addresses; configure `trusted_hosts` to allow it"
                        .to_owned(),
                }
                .into())
            } else {
                Ok(Box::new(kept.into_iter()) as Addrs)
            }
        })
    }
}

fn retain_allowed(trusted: bool, addrs: Addrs) -> Vec<SocketAddr> {
    addrs.filter(|addr| trusted || is_global_ip(addr.ip())).collect()
}

const fn literal_ip(host: Option<&Host<&str>>) -> Option<IpAddr> {
    match host {
        Some(Host::Ipv4(ip)) => Some(IpAddr::V4(*ip)),
        Some(Host::Ipv6(ip)) => Some(IpAddr::V6(*ip)),
        Some(Host::Domain(_)) | None => None,
    }
}

fn canonical_entry(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    let inner = entry
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(entry);
    if let Ok(ip) = inner.parse::<IpAddr>() {
        return Some(ip.to_string());
    }
    Some(entry.to_ascii_lowercase())
}

const fn is_global_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_global_ipv4(ip),
        IpAddr::V6(ip) => is_global_ipv6(ip),
    }
}

const fn is_global_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    if a == 0 {
        return false;
    }
    if ip.is_private() {
        return false;
    }
    if is_shared_ipv4(ip) {
        return false;
    }
    if ip.is_loopback() {
        return false;
    }
    if ip.is_link_local() {
        return false;
    }
    if ip.is_multicast() {
        return false;
    }
    if ip.is_broadcast() {
        return false;
    }
    if ip.is_documentation() {
        return false;
    }
    if is_benchmarking_ipv4(ip) {
        return false;
    }
    if a == 192 && b == 0 {
        return false;
    }
    a < 240
}

const fn is_shared_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (b & 0b1100_0000 == 0b0100_0000)
}

const fn is_benchmarking_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 198 && (b & 0xfe == 18)
}

// IANA registry snapshot 2025-10-09:
// https://www.iana.org/assignments/iana-ipv6-special-registry/iana-ipv6-special-registry.xhtml
const IPV6_GLOBAL_EXCEPTIONS: &[(Ipv6Addr, u32)] = &[
    (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 1), 128),
    (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 2), 128),
    (Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 3), 128),
    (Ipv6Addr::new(0x2001, 4, 0x112, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x2001, 3, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2001, 0x20, 0, 0, 0, 0, 0, 0), 28),
    (Ipv6Addr::new(0x2001, 0x30, 0, 0, 0, 0, 0, 0), 28),
];

const IPV6_BLOCKED_PREFIXES: &[(Ipv6Addr, u32)] = &[
    (Ipv6Addr::UNSPECIFIED, 128),
    (Ipv6Addr::LOCALHOST, 128),
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x100, 0, 0, 1, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    // RFC 3879 acknowledges that deployments may still route deprecated site-local addresses:
    // https://www.rfc-editor.org/rfc/rfc3879.html
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

const fn is_global_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_global_ipv4(mapped);
    }
    matches_any_ipv6_prefix(ip, IPV6_GLOBAL_EXCEPTIONS) || !matches_any_ipv6_prefix(ip, IPV6_BLOCKED_PREFIXES)
}

const fn matches_any_ipv6_prefix(ip: Ipv6Addr, prefixes: &[(Ipv6Addr, u32)]) -> bool {
    let address = u128::from_be_bytes(ip.octets());
    let mut index = 0;
    while index < prefixes.len() {
        let (network, prefix_len) = prefixes[index];
        let mask = u128::MAX << (128 - prefix_len);
        if address & mask == u128::from_be_bytes(network.octets()) & mask {
            return true;
        }
        index += 1;
    }
    false
}

struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs = tokio::net::lookup_host((host, 0)).await?;
            Ok(Box::new(addrs) as Addrs)
        })
    }
}

#[cfg(test)]
#[path = "../../tests/unit/client/guard_tests.rs"]
mod tests;
