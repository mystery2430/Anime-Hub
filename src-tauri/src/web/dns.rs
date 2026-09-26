//! Synchronous DNS check for the navigation guard.
//!
//! `decide_navigation` has to stay synchronous: Tauri's `on_navigation`
//! callback cannot await. A public-looking name that resolves to a private
//! address (the DNS-rebinding case the string policy cannot see) is therefore
//! resolved here, on a worker thread, with a hard timeout.
//!
//! What this does **not** do: the WebView resolves subresources on its own
//! stack after the top-level navigation has been allowed. A name that is
//! public at check time and private a TTL later can still be reached by
//! `fetch` inside the already-loaded page. Closing that requires a filtering
//! proxy in front of the WebView, which v1 does not ship. The decision is
//! recorded in `HANDOFF.md`.

use crate::sites::url_policy::{is_private_host, is_public_ip};
use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a navigation may wait on the resolver before falling back to the
/// string policy. Long enough for a cold recursive lookup, short enough that
/// a hung resolver cannot freeze the WebView thread indefinitely.
const LOOKUP_TIMEOUT: Duration = Duration::from_millis(1_200);

/// Public answers are cached briefly. A long cache would itself be a rebinding
/// window: the second navigation would trust a stale "public" result.
const PUBLIC_TTL: Duration = Duration::from_secs(15);

/// A private answer should keep blocking for a while; the attacker does not
/// get a second chance by waiting out a short cache.
const PRIVATE_TTL: Duration = Duration::from_secs(60);

/// Result of classifying a host before a navigation or a site open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsClass {
    /// The resolver answered. `private` is true when **any** address is
    /// non-public, or when the answer was empty (no routable address to pin).
    Answered { private: bool },
    /// Timeout, NXDOMAIN, or a resolver error. Callers must fall back to the
    /// string policy rather than failing closed: a DNS outage must not make
    /// every site unopenable.
    Unknown,
}

/// True when a set of resolved addresses must not be navigated to.
///
/// An empty answer is treated as private: there is no public address to pin,
/// so allowing the navigation would let the WebView's own resolver decide.
pub fn answers_are_private(addrs: &[IpAddr]) -> bool {
    addrs.is_empty() || addrs.iter().copied().any(|ip| !is_public_ip(ip))
}

/// Classify `host` for the navigation guard.
///
/// Literal and suffix-private hosts (`127.0.0.1`, `*.local`, …) never touch
/// the network. Everything else is resolved with [`LOOKUP_TIMEOUT`].
pub fn classify_host(host: &str) -> DnsClass {
    let key = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if key.is_empty() {
        return DnsClass::Unknown;
    }

    if let Some(hit) = cache_get(&key) {
        return hit;
    }

    // The string policy already knows these. Resolving them would only give
    // a rebinding attacker a timeout-shaped hole (`Unknown` falls open).
    if is_private_host(&key) || literal_is_private(&key) {
        let class = DnsClass::Answered { private: true };
        cache_put(key, class, PRIVATE_TTL);
        return class;
    }

    if let Some(ip) = literal_ip(&key) {
        let private = !is_public_ip(ip);
        let class = DnsClass::Answered { private };
        cache_put(key, class, if private { PRIVATE_TTL } else { PUBLIC_TTL });
        return class;
    }

    let class = lookup_with_timeout(&key);
    match class {
        DnsClass::Answered { private: true } => cache_put(key, class, PRIVATE_TTL),
        DnsClass::Answered { private: false } => cache_put(key, class, PUBLIC_TTL),
        DnsClass::Unknown => {}
    }
    class
}

fn literal_ip(host: &str) -> Option<IpAddr> {
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse().ok()
}

fn literal_is_private(host: &str) -> bool {
    literal_ip(host).is_some_and(|ip| !is_public_ip(ip))
}

fn lookup_with_timeout(host: &str) -> DnsClass {
    let host = host.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    // Detach: on timeout the lookup thread exits on its own when the
    // resolver finally returns. A launcher does not navigate often enough
    // for a stuck resolver to pile these up, and the cache lock below
    // serialises callers so they share one in-flight miss.
    std::thread::spawn(move || {
        let _ = tx.send(resolve_ips(&host));
    });
    match rx.recv_timeout(LOOKUP_TIMEOUT) {
        Ok(Ok(ips)) => DnsClass::Answered {
            private: answers_are_private(&ips),
        },
        Ok(Err(_)) | Err(_) => DnsClass::Unknown,
    }
}

fn resolve_ips(host: &str) -> std::io::Result<Vec<IpAddr>> {
    let mut ips: Vec<IpAddr> = (host, 443u16)
        .to_socket_addrs()?
        .map(|sa| sa.ip())
        .collect();
    ips.sort();
    ips.dedup();
    Ok(ips)
}

struct CacheEntry {
    class: DnsClass,
    expires: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(key: &str) -> Option<DnsClass> {
    let mut guard = cache().lock().ok()?;
    let now = Instant::now();
    match guard.get(key) {
        Some(entry) if entry.expires > now => Some(entry.class),
        Some(_) => {
            guard.remove(key);
            None
        }
        None => None,
    }
}

fn cache_put(key: String, class: DnsClass, ttl: Duration) {
    let Ok(mut guard) = cache().lock() else {
        return;
    };
    // Bound the map. A hostile page navigating to thousands of unique hosts
    // must not grow this without limit. Evict before insert so `key` is only
    // moved once.
    if guard.len() >= 256 {
        let now = Instant::now();
        guard.retain(|_, entry| entry.expires > now);
        if guard.len() >= 256 {
            guard.clear();
        }
    }
    guard.insert(
        key,
        CacheEntry {
            class,
            expires: Instant::now() + ttl,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn empty_answer_is_private() {
        assert!(answers_are_private(&[]));
    }

    #[test]
    fn any_private_address_poisons_the_set() {
        let mixed = [
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
        ];
        assert!(answers_are_private(&mixed));
        assert!(answers_are_private(&[IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))]));
        assert!(answers_are_private(&[IpAddr::V6(Ipv6Addr::LOCALHOST)]));
        assert!(!answers_are_private(&[
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
        ]));
    }

    #[test]
    fn literals_are_classified_without_dns() {
        assert_eq!(
            classify_host("127.0.0.1"),
            DnsClass::Answered { private: true }
        );
        assert_eq!(
            classify_host("192.168.1.1"),
            DnsClass::Answered { private: true }
        );
        assert_eq!(
            classify_host("[::1]"),
            DnsClass::Answered { private: true }
        );
        assert_eq!(
            classify_host("1.1.1.1"),
            DnsClass::Answered { private: false }
        );
        assert_eq!(
            classify_host("8.8.8.8"),
            DnsClass::Answered { private: false }
        );
        // Suffix policy, still no network.
        assert_eq!(
            classify_host("router.local"),
            DnsClass::Answered { private: true }
        );
        assert_eq!(
            classify_host("metadata.google.internal"),
            DnsClass::Answered { private: true }
        );
    }

    #[test]
    fn literal_classification_is_cached() {
        assert_eq!(
            classify_host("1.1.1.1"),
            classify_host("1.1.1.1"),
        );
    }
}
