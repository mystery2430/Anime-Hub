//! HTTPS-only URL validation and sanitisation.
//!
//! This module is the single choke point every user-supplied URL passes
//! through: the "add site" form, `open_site`, and the navigation guard on
//! every WebView. Nothing reaches a WebView without going through
//! [`validate_site_url`] first.

use url::Url;

/// Schemes we refuse outright. `http` is rejected (not upgraded) so the user
/// is never silently sent somewhere other than where they typed.
const DENIED_SCHEMES: &[&str] = &[
    "http",
    "javascript",
    "data",
    "blob",
    "file",
    "about",
    "intent",
    "chrome",
    "view-source",
    "resource",
];

/// A URL that survived validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeUrl {
    url: Url,
    host: String,
}

impl SafeUrl {
    pub fn as_url(&self) -> &Url {
        &self.url
    }

    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    /// Registered domain-ish host, lowercased and punycode-encoded by `url`.
    pub fn host(&self) -> &str {
        &self.host
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    Unparsable,
    NotHttpLike,
    InsecureScheme,
    MissingHost,
    LocalOrPrivateHost,
    CredentialsInUrl,
}

impl Rejection {
    pub fn reason(self) -> &'static str {
        match self {
            Rejection::Unparsable => "adres çözümlenemedi",
            Rejection::NotHttpLike => "adres http(s) değil",
            Rejection::InsecureScheme => "yalnızca HTTPS adresler kabul edilir",
            Rejection::MissingHost => "adresin bir alan adı yok",
            Rejection::LocalOrPrivateHost => "yerel/ağ içi adresler kabul edilmez",
            Rejection::CredentialsInUrl => "adrese kullanıcı adı/parola gömülemez",
        }
    }
}

/// Validate a URL intended to be stored as a site entry point.
///
/// Rules, in order:
/// 1. must parse with `url` (which also punycode-normalises the host),
/// 2. scheme must be `https`,
/// 3. host must exist and be a public host (no loopback/private/link-local,
///    which stops a "site" from being pointed at the user's own router or at
///    `tauri.localhost` / other internal surfaces),
/// 4. embedded credentials are stripped-refused,
/// 5. the URL is re-serialised with a trailing `/` on the root so that
///    string comparisons in the registry are stable.
pub fn validate_site_url(raw: &str) -> Result<SafeUrl, Rejection> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Rejection::Unparsable);
    }

    let mut url = Url::parse(trimmed).map_err(|_| Rejection::Unparsable)?;

    // Only http/https can be meaningfully hosted in a WebView; everything
    // else is refused before we even look at the scheme allow-list.
    match url.scheme() {
        "https" => {}
        "http" => return Err(Rejection::InsecureScheme),
        _ => {
            if DENIED_SCHEMES.contains(&url.scheme()) {
                return Err(Rejection::InsecureScheme);
            }
            return Err(Rejection::NotHttpLike);
        }
    }

    let host = url
        .host_str()
        .map(str::to_lowercase)
        .filter(|h| !h.is_empty())
        .ok_or(Rejection::MissingHost)?;

    // Checked before anything else that could mask the reason: pointing a
    // "site" at localhost or a private range is the attack this guards.
    if is_private_host(&host) {
        return Err(Rejection::LocalOrPrivateHost);
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(Rejection::CredentialsInUrl);
    }

    // Normalise: drop any fragment (a site entry point has no business
    // carrying one) and ensure a root path so `https://a.example` and
    // `https://a.example/` compare equal.
    url.set_fragment(None);
    if url.path().is_empty() {
        url.set_path("/");
    }

    Ok(SafeUrl { url, host })
}

/// Why a navigation was refused.
///
/// The codes are stable strings, shared with the Android policy through
/// `tests/fixtures/navigation-policy.tsv`: both implementations have to answer
/// with the same allow/block decision *and* the same reason for every vector,
/// or the two platforms drift apart silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavBlock {
    /// `http://` — a downgrade, blocked rather than silently upgraded.
    InsecureScheme,
    /// Anything that is not https/blob/about (file:, javascript:, ftp:, …).
    UnsupportedScheme,
    /// A top-level `data:` document. See [`navigation_rejection`].
    DataNavigation,
    /// An `about:` page other than `about:blank`.
    UnknownAbout,
    /// Loopback, private, link-local, reserved or otherwise non-routable host.
    PrivateHost,
    /// An https URL without a host at all.
    MissingHost,
}

impl NavBlock {
    /// Machine-readable code used by logs and by the shared policy vectors.
    pub fn code(self) -> &'static str {
        match self {
            NavBlock::InsecureScheme => "insecure_scheme",
            NavBlock::UnsupportedScheme => "unsupported_scheme",
            NavBlock::DataNavigation => "data_navigation",
            NavBlock::UnknownAbout => "unknown_about",
            NavBlock::PrivateHost => "private_host",
            NavBlock::MissingHost => "missing_host",
        }
    }
}

/// Guard for in-WebView navigation: `None` means allowed, `Some(code)` why not.
///
/// This is a **top-level navigation** checkpoint (Tauri's `on_navigation`, and
/// `SiteNavigationGuard.shouldOverrideUrlLoading` on Android). Sub-resources
/// never pass through it, which is why `blob:` stays allowed — MSE players
/// navigate the main frame to `blob:` object URLs, and blocking that breaks
/// playback — and why `data:` is now *refused*: as a top-level document a
/// `data:` URL is an opaque-origin HTML/script carrier (a "fake page" that no
/// site legitimately needs), while the app's own `data:image/…` logos are
/// sub-resources and are unaffected. `about:` is narrowed to `about:blank`,
/// the only page a player or popup has a real reason to open.
///
/// Stricter than [`validate_site_url`]: an `http` navigation is *denied* rather
/// than reported as "not configured".
pub fn navigation_rejection(url: &Url) -> Option<NavBlock> {
    match url.scheme() {
        "https" => {}
        "http" => return Some(NavBlock::InsecureScheme),
        "blob" => {
            // `blob:https://host/uuid` inherits its creator's origin; a blob
            // created over plain http is a downgrade in disguise.
            let inner = url.path();
            if inner.starts_with("http://") {
                return Some(NavBlock::InsecureScheme);
            }
            return None;
        }
        "data" => return Some(NavBlock::DataNavigation),
        "about" => {
            return if url.path() == "blank" {
                None
            } else {
                Some(NavBlock::UnknownAbout)
            }
        }
        _ => return Some(NavBlock::UnsupportedScheme),
    }

    match url.host_str() {
        None => Some(NavBlock::MissingHost),
        Some(h) => {
            let h = h.to_lowercase();
            if h == "localhost" || is_private_host(&h) {
                Some(NavBlock::PrivateHost)
            } else {
                None
            }
        }
    }
}

/// Guard for in-WebView navigation. Thin wrapper over [`navigation_rejection`];
/// callers that need to log or report a reason should use that instead.
pub fn navigation_allowed(url: &Url) -> bool {
    navigation_rejection(url).is_none()
}

/// True when a host's LAST label makes browsers run the IPv4 parser on it
/// (all digits, or `0x`-prefixed hex) — the WHATWG "ends in a number" rule.
fn ends_in_a_number(host: &str) -> bool {
    let last = host.rsplit('.').next().unwrap_or(host);
    if last.is_empty() {
        return false;
    }
    if last.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    // "0x" on its own counts too (WHATWG allows an empty hex part, and
    // `SiteNavigationPolicy`'s regex accepts it) — it is not a valid IPv4
    // literal, so the fail-closed branch below refuses it on both platforms.
    let rest = match last.get(2..) {
        Some(rest) if last[..2].eq_ignore_ascii_case("0x") => rest,
        _ => return false,
    };
    rest.bytes().all(|b| b.is_ascii_hexdigit())
}

/// True for loopback, private, link-local and other non-routable hosts.
///
/// Public because the in-WebView navigation guard needs the same rule:
/// [`validate_site_url`] protects the *stored* entry point, but a site is free
/// to redirect the WebView to `https://192.168.1.1/` afterwards, and that
/// navigation never passes through `validate_site_url` again.
pub fn is_private_host(host: &str) -> bool {
    // Lower-case here rather than trusting every caller: the shared vectors feed
    // `LOCALHOST` and `openani.me.` straight in, and the Android rule lower-cases
    // and trims internally too.
    let lowered = host.to_lowercase();
    let h = lowered.trim_end_matches('.');
    // No host at all is not a destination we can reason about: fail closed.
    if h.is_empty() {
        return true;
    }

    // Literal IPv6 in `url` keeps the brackets.
    let bare = h
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(h);
    // If it looks like an IP literal but fails to parse, treat as private (fail-closed).
    if bare.contains(':') {
        return match bare.parse::<std::net::IpAddr>() {
            Ok(ip) => !ip_is_public(&ip),
            Err(_) => true,
        };
    }
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return !ip_is_public(&ip);
    }

    // A host that "ends in a number" (`0x7f000001`, `2130706433`, `127.1`,
    // `010.1`, `0177.0.0.1`, `example.0x7f`) goes through the WHATWG IPv4
    // parser, exactly like a browser and like the url crate do for full URLs.
    // Reuse the crate's parser instead of writing a second one that could
    // drift, and fail closed when it refuses the host: Android's `Uri` does
    // NOT normalise these, so `SiteNavigationPolicy` runs the same parser in
    // Kotlin and the shared vectors pin the two against each other.
    if ends_in_a_number(bare) {
        return match Url::parse(&format!("https://{bare}/")) {
            Ok(parsed) => match parsed.host() {
                Some(url::Host::Ipv4(v4)) => !ip_is_public(&std::net::IpAddr::V4(v4)),
                Some(url::Host::Ipv6(v6)) => !ip_is_public(&std::net::IpAddr::V6(v6)),
                // Not an IP literal the parser accepted (`example.0x7f`).
                _ => true,
            },
            Err(_) => true,
        };
    }

    // Hostname suffixes that resolve to the machine itself.
    matches!(
        h,
        "localhost"
            | "localhost.localdomain"
            | "ip6-localhost"
            | "ip6-loopback"
            | "tauri.localhost"
            | "asset.localhost"
    ) || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
        || h.ends_with(".lan")
        || h.ends_with(".home")
        || h == "metadata.google.internal"
}

/// True when `ip` is a globally routable address.
///
/// Shared with the DNS-rebinding check: a hostname is refused when **any**
/// of its answers fails this test.
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    ip_is_public(&ip)
}

fn ip_is_public(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, c, d] = v4.octets();
            // IANA IPv4 Special-Purpose Registry (2025-10-09) – Globally Reachable == false
            // plus multicast/reserved. Keep in step with Kotlin SiteNavigationPolicy.
            // 192.0.0.0/24 has exceptions 192.0.0.9/32 and 192.0.0.10/32 (true).
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || a == 0 // 0.0.0.0/8 This network
                // 100.64.0.0/10 CGNAT
                || (a == 100 && (64..=127).contains(&b))
                // 192.0.0.0/24 IANA reserved, but 192.0.0.9 and .10 are globally reachable (RFC 7723/8155)
                || (a == 192 && b == 0 && c == 0 && d != 9 && d != 10)
                // 192.0.2.0/24 TEST-NET-1
                || (a == 192 && b == 0 && c == 2)
                // 192.88.99.0/24 6to4 relay (deprecated)
                || (a == 192 && b == 88 && c == 99)
                // 198.18.0.0/15 benchmarking
                || (a == 198 && (18..=19).contains(&b))
                // 224.0.0.0/4 multicast, 240.0.0.0/4 reserved/broadcast
                || a >= 224)
        }
        std::net::IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            let seg = v6.segments();
            // 3fff::/20 documentation (RFC 9637) – first 20 bits 0011 1111 1111 1111 0000
            if seg[0] == 0x3fff && (seg[1] & 0xf000) == 0x0000 {
                return false;
            }
            // 5f00::/16 SRv6 SIDs
            if seg[0] == 0x5f00 {
                return false;
            }
            // 64:ff9b:1::/48 translation – false (must be before /96 handling)
            if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2] == 0x0001 {
                return false;
            }
            // 100::/64 discard-only
            if seg[0] == 0x0100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 {
                return false;
            }
            // 100:0:0:1::/64 dummy
            if seg[0] == 0x0100 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0x0001 {
                return false;
            }
            // 2001:db8::/32 documentation
            if seg[0] == 0x2001 && seg[1] == 0x0db8 {
                return false;
            }
            // 2001:2::/48 benchmarking
            if seg[0] == 0x2001 && seg[1] == 0x0002 && seg[2] == 0x0000 {
                return false;
            }
            // 2002::/16 6to4 – deprecated
            if seg[0] == 0x2002 {
                return false;
            }
            // fec0::/10 site-local – deprecated (RFC 3879)
            if (seg[0] & 0xffc0) == 0xfec0 {
                return false;
            }
            // 2001::/23 IETF protocol assignments – false unless more-specific true allocation
            // 2001::/32 Teredo has N/A, so NOT whitelisted (fail-closed)
            if seg[0] == 0x2001 && (seg[1] & 0xfe00) == 0x0000 {
                // Whitelist only allocations with Globally Reachable == true
                let is_pcp1 = seg[0] == 0x2001
                    && seg[1] == 0x0001
                    && seg[2] == 0x0000
                    && seg[3] == 0x0000
                    && seg[4] == 0x0000
                    && seg[5] == 0x0000
                    && seg[6] == 0x0000
                    && seg[7] == 0x0001;
                let is_pcp2 = seg[0] == 0x2001
                    && seg[1] == 0x0001
                    && seg[2] == 0x0000
                    && seg[3] == 0x0000
                    && seg[4] == 0x0000
                    && seg[5] == 0x0000
                    && seg[6] == 0x0000
                    && seg[7] == 0x0002;
                let is_pcp3 = seg[0] == 0x2001
                    && seg[1] == 0x0001
                    && seg[2] == 0x0000
                    && seg[3] == 0x0000
                    && seg[4] == 0x0000
                    && seg[5] == 0x0000
                    && seg[6] == 0x0000
                    && seg[7] == 0x0003;
                let is_amt = seg[0] == 0x2001 && seg[1] == 0x0003; // 2001:3::/32
                let is_as112 = seg[0] == 0x2001 && seg[1] == 0x0004 && seg[2] == 0x0112; // 2001:4:112::/48
                                                                                         // ORCHIDv2 2001:20::/28 and Drone 2001:30::/28 are true;
                                                                                         // 2001:10::/28 is deprecated and must stay blocked.
                let is_orchid = seg[0] == 0x2001
                    && ((seg[1] & 0xfff0) == 0x0020 || (seg[1] & 0xfff0) == 0x0030);
                let allowed = is_pcp1 || is_pcp2 || is_pcp3 || is_amt || is_as112 || is_orchid;
                if !allowed {
                    return false;
                }
            }
            // fe80::/10 link-local, fc00::/7 unique-local
            if (seg[0] & 0xffc0) == 0xfe80 || (seg[0] & 0xfe00) == 0xfc00 {
                return false;
            }
            // IPv4-embedded: ::ffff:0:0/96 (mapped) and ::/96 (compatible, deprecated)
            // These are outside 2000::/3 but are handled as special cases:
            // allow only if embedded IPv4 is public, otherwise block.
            if let Some(m) = v6.to_ipv4_mapped() {
                return ip_is_public(&std::net::IpAddr::V4(m));
            } else if seg[0] == 0
                && seg[1] == 0
                && seg[2] == 0
                && seg[3] == 0
                && seg[4] == 0
                && seg[5] == 0
                && (seg[6] != 0 || seg[7] != 0)
            {
                let v4 = std::net::Ipv4Addr::new(
                    ((seg[6] >> 8) & 0xff) as u8,
                    (seg[6] & 0xff) as u8,
                    ((seg[7] >> 8) & 0xff) as u8,
                    (seg[7] & 0xff) as u8,
                );
                return ip_is_public(&std::net::IpAddr::V4(v4));
            }
            // 64:ff9b::/96 translation – globally reachable true per IANA,
            // but the embedded IPv4 must itself be globally reachable (RFC 6052).
            // If the last 32 bits encode a private/reserved IPv4, the whole
            // address is not globally reachable.
            if seg[0] == 0x0064
                && seg[1] == 0xff9b
                && seg[2] == 0
                && seg[3] == 0
                && seg[4] == 0
                && seg[5] == 0
            {
                let v4 = std::net::Ipv4Addr::new(
                    ((seg[6] >> 8) & 0xff) as u8,
                    (seg[6] & 0xff) as u8,
                    ((seg[7] >> 8) & 0xff) as u8,
                    (seg[7] & 0xff) as u8,
                );
                if !ip_is_public(&std::net::IpAddr::V4(v4)) {
                    return false;
                }
            }
            // Fail-closed: only 2000::/3 is global-unicast. Outside that,
            // only 64:ff9b::/96 with public IPv4 (handled above) is allowed.
            if seg[0] == 0x0064
                && seg[1] == 0xff9b
                && seg[2] == 0
                && seg[3] == 0
                && seg[4] == 0
                && seg[5] == 0
            {
                return true;
            }
            if (seg[0] & 0xe000) != 0x2000 {
                return false;
            }
            true
        }
    }
}

/// Strip anything that could be used for header/URL injection if the value is
/// ever echoed into a `Set-Cookie` or a log line.
pub fn sanitize_cookie_component(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() && !matches!(c, ';' | ',' | ' ' | '"' | '\\'))
        .take(4096)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_https() {
        let s = validate_site_url("https://openani.me").expect("valid");
        assert_eq!(s.host(), "openani.me");
        assert_eq!(s.as_str(), "https://openani.me/");
    }

    #[test]
    fn accepts_https_with_path_and_query() {
        let s = validate_site_url("https://animecix.tv/series/x?tab=1#frag").expect("valid");
        assert_eq!(s.as_str(), "https://animecix.tv/series/x?tab=1");
        assert_eq!(s.host(), "animecix.tv");
    }

    #[test]
    fn rejects_http_downgrade() {
        assert_eq!(
            validate_site_url("http://openani.me"),
            Err(Rejection::InsecureScheme)
        );
    }

    #[test]
    fn rejects_dangerous_schemes() {
        for u in [
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "blob:https://x/y",
        ] {
            assert!(
                matches!(
                    validate_site_url(u),
                    Err(Rejection::InsecureScheme)
                        | Err(Rejection::NotHttpLike)
                        | Err(Rejection::Unparsable)
                ),
                "{u} should be refused"
            );
        }
    }

    #[test]
    fn rejects_localhost_and_private_hosts() {
        for u in [
            "https://localhost/x",
            "https://127.0.0.1/x",
            "https://192.168.1.1/",
            "https://10.0.0.5/",
            "https://172.16.4.4/",
            "https://169.254.169.254/latest/meta-data/",
            "https://[::1]/",
            "https://tauri.localhost/",
            "https://router.lan/",
            "https://nas.home/",
        ] {
            assert_eq!(
                validate_site_url(u),
                Err(Rejection::LocalOrPrivateHost),
                "{u} must not be storable"
            );
        }
    }

    #[test]
    fn accepts_public_ipv4_and_ipv6() {
        assert!(validate_site_url("https://1.1.1.1/").is_ok());
        assert!(validate_site_url("https://[2606:4700:4700::1111]/").is_ok());
    }

    #[test]
    fn cgnat_range_is_rejected() {
        assert_eq!(
            validate_site_url("https://100.64.0.1/"),
            Err(Rejection::LocalOrPrivateHost)
        );
        assert_eq!(
            validate_site_url("https://100.127.255.255/"),
            Err(Rejection::LocalOrPrivateHost)
        );
        // 100.128.0.0 is outside CGNAT and must be accepted.
        assert!(validate_site_url("https://100.128.0.1/").is_ok());
    }

    #[test]
    fn rejects_embedded_credentials() {
        assert_eq!(
            validate_site_url("https://user:pass@example.com/"),
            Err(Rejection::CredentialsInUrl)
        );
    }

    #[test]
    fn unicode_host_is_punycode_normalised() {
        // Exact mapping depends on the IDNA version used by `url`, so only
        // assert the properties we rely on: ASCII, and flagged as punycode.
        let s = validate_site_url("https://例え.テスト/").expect("valid");
        assert!(s.host().is_ascii(), "host must be ASCII: {}", s.host());
        assert!(s.host().starts_with("xn--"), "host: {}", s.host());
    }

    #[test]
    fn empty_and_whitespace_input_is_rejected() {
        assert_eq!(validate_site_url(""), Err(Rejection::Unparsable));
        assert_eq!(validate_site_url("   "), Err(Rejection::Unparsable));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let s = validate_site_url("  https://openani.me/  ").expect("valid");
        assert_eq!(s.as_str(), "https://openani.me/");
    }

    #[test]
    fn trailing_dot_is_stripped_for_private_check() {
        assert_eq!(
            validate_site_url("https://localhost./"),
            Err(Rejection::LocalOrPrivateHost)
        );
    }

    #[test]
    fn navigation_blocks_http_downgrade() {
        assert!(!navigation_allowed(
            &Url::parse("http://openani.me/").unwrap()
        ));
        assert!(navigation_allowed(
            &Url::parse("https://openani.me/").unwrap()
        ));
    }

    #[test]
    fn navigation_allows_media_schemes_but_not_private_hosts() {
        assert!(navigation_allowed(
            &Url::parse("blob:https://openani.me/abc").unwrap()
        ));
        assert!(!navigation_allowed(
            &Url::parse("https://127.0.0.1/x").unwrap()
        ));
        assert!(!navigation_allowed(
            &Url::parse("ftp://openani.me/").unwrap()
        ));
    }

    #[test]
    fn cookie_component_strips_separators() {
        assert_eq!(
            sanitize_cookie_component("a; Path=/ b\"c\\d\ne"),
            "aPath=/bcde"
        );
    }

    #[test]
    fn ipv4_iana_precise_blocks_and_allows() {
        // Blocked: IANA false
        for ip in [
            "0.0.0.1",
            "10.5.6.7",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.10.20",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.0.8",
            "192.0.0.170",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.51.100.1",
            "203.0.113.1",
            "198.18.0.1",
            "198.19.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            let addr: std::net::IpAddr = ip.parse().unwrap();
            assert!(!is_public_ip(addr), "should block {ip}");
            assert!(is_private_host(ip), "host {ip} should be private");
        }
        // Allowed: exceptions and public
        for ip in [
            "192.0.0.9",
            "192.0.0.10",
            "192.0.1.1",
            "192.0.3.1",
            "192.1.0.1",
            "8.8.8.8",
            "1.1.1.1",
            "100.128.0.1",
            "198.20.0.1",
            "192.0.4.1",
        ] {
            let addr: std::net::IpAddr = ip.parse().unwrap();
            assert!(is_public_ip(addr), "should allow {ip}");
            assert!(!is_private_host(ip), "host {ip} should not be private");
            assert!(
                validate_site_url(&format!("https://{ip}/")).is_ok(),
                "https://{ip}/ should validate"
            );
        }
    }

    #[test]
    fn ipv6_iana_blocks_and_allows() {
        // Blocked vectors (must be rejected) – RFC 6052 and IANA 2025-10-09
        // Includes fail-closed cases outside 2000::/3 and deprecated 6to4/site-local.
        for ip in [
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:192.168.1.1",
            "::ffff:10.0.0.1",
            "::10.0.0.1",
            "::7f00:1",
            "::192.168.1.1",
            "fe80::1",
            "fc00::1",
            "ff02::1",
            "ff00::1",
            "2001:db8::1",
            "100::1",
            "100:0:0:1::1",
            "3fff::1",
            "3fff:0fff::1", // inside 3fff::/20 near boundary (high nibble 0x0)
            "64:ff9b:1::1",
            "64:ff9b:1::192.0.2.1",
            // NAT64 /96 with non-global embedded IPv4 must be rejected (RFC 6052)
            "64:ff9b::10.0.0.1",
            "64:ff9b::192.168.1.1",
            "64:ff9b::192.0.2.1",
            "2001:2::1",
            "5f00::1",
            "2001:5::1",
            "2001:0:5::1",
            // 2001:10::/28 is deprecated (ex-ORCHID) – not globally reachable
            "2001:10::1",
            "2001:10:ffff::1",
            // Teredo 2001::/32 has N/A, so fail-closed (not explicitly true)
            "2001::1",
            "2001:0:0:1::1",
            "2001:0:1::1",
            "2001:1:ffff::1",
            // 6to4, site-local, and non-global-unicast
            "2002::1",
            "fec0::1",
            "4000::1",
        ] {
            let addr: std::net::IpAddr = ip.parse().unwrap_or_else(|_| panic!("parse {ip}"));
            assert!(!is_public_ip(addr), "should block {ip}");
            assert!(
                is_private_host(&format!("[{ip}]")),
                "host [{ip}] should be private"
            );
        }
        // Allowed vectors (must pass) – more-specific true allocations inside 2001::/23
        // Note: 2001::/32 Teredo is N/A, so NOT allowed (fail-closed).
        // 3fff:1234::1 is outside 3fff::/20 (high nibble 0x1), so it is global-unicast and allowed.
        for ip in [
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            // NAT64 /96 with global embedded IPv4 is allowed
            "64:ff9b::8.8.8.8",
            "64:ff9b::1.1.1.1",
            "64:ff9b::192.0.1.1",
            "3fff:1234::1", // outside 3fff::/20 (high nibble 0x1), so public
            "2001:1::1",
            "2001:1::2",
            "2001:1::3",
            "2001:3::1",
            "2001:4:112::1",
            "2001:20::1",
            "2001:20:0:1::1",
            "2001:2f:ff::1", // still inside 2001:20::/28 (mask 0xfff0)
            "2001:30::1",
            "2001:30:ffff::1",
            "2600::1",
        ] {
            let addr: std::net::IpAddr = match ip.parse() {
                Ok(a) => a,
                Err(_) => continue,
            };
            assert!(is_public_ip(addr), "should allow {ip}");
            assert!(
                !is_private_host(&format!("[{ip}]")),
                "host [{ip}] should not be private"
            );
            // Also ensure validate_site_url allows it when used as host
            assert!(
                validate_site_url(&format!("https://[{ip}]/")).is_ok(),
                "https://[{ip}]/ should validate"
            );
        }
    }

    #[test]
    fn nat64_inner_ipv4_is_checked() {
        // RFC 6052: embedded IPv4 must be checked via same public-ip policy.
        for ip in [
            "64:ff9b::10.0.0.1",
            "64:ff9b::192.168.1.1",
            "64:ff9b::192.0.2.1",
        ] {
            let addr: std::net::IpAddr = ip.parse().unwrap();
            assert!(
                !is_public_ip(addr),
                "NAT64 with private inner should block {ip}"
            );
        }
        for ip in ["64:ff9b::8.8.8.8", "64:ff9b::1.1.1.1", "64:ff9b::192.0.1.1"] {
            let addr: std::net::IpAddr = ip.parse().unwrap();
            assert!(
                is_public_ip(addr),
                "NAT64 with public inner should allow {ip}"
            );
        }
        // /48 is always blocked regardless of inner
        let a48: std::net::IpAddr = "64:ff9b:1::8.8.8.8".parse().unwrap();
        assert!(!is_public_ip(a48), "64:ff9b:1::/48 must stay blocked");
    }

    #[test]
    fn ipv4_mapped_allows_public_blocks_private() {
        // Rust and Kotlin must agree on mapped: public inner → allow, private inner → block
        let pub_mapped: std::net::IpAddr = "::ffff:8.8.8.8".parse().unwrap();
        let priv_mapped: std::net::IpAddr = "::ffff:192.168.1.1".parse().unwrap();
        let priv_mapped2: std::net::IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        assert!(
            is_public_ip(pub_mapped),
            "::ffff:8.8.8.8 should be public (inner 8.8.8.8 public)"
        );
        assert!(
            !is_public_ip(priv_mapped),
            "::ffff:192.168.1.1 should be private (inner private)"
        );
        assert!(
            !is_public_ip(priv_mapped2),
            "::ffff:10.0.0.1 should be private"
        );
        // Also via is_private_host (brackets)
        assert!(!is_private_host("[::ffff:8.8.8.8]"));
        assert!(is_private_host("[::ffff:192.168.1.1]"));
    }

    #[test]
    fn orchid_v2_mask_is_correct() {
        // 2001:20::/28 covers 0020-002f, 2001:30::/28 covers 0030-003f.
        // 2001:10::/28 (deprecated) must stay blocked.
        assert!(is_public_ip("2001:20::1".parse().unwrap()));
        assert!(is_public_ip("2001:2f:ffff::1".parse().unwrap())); // 002f & fff0 = 0020
        assert!(is_public_ip("2001:30::1".parse().unwrap()));
        assert!(is_public_ip("2001:3f::1".parse().unwrap())); // 003f & fff0 = 0030
        assert!(!is_public_ip("2001:10::1".parse().unwrap()));
        assert!(!is_public_ip("2001:1f::1".parse().unwrap())); // 001f & fff0 = 0010 -> deprecated
        assert!(!is_public_ip("2001:40::1".parse().unwrap())); // outside both
                                                               // 2001:1::3 is a distinct /128, not part of /28
        assert!(is_public_ip("2001:1::3".parse().unwrap()));
    }

    #[test]
    fn is_private_host_rejects_invalid_ipv6() {
        // Invalid literals must be blocked (fail-closed)
        assert!(is_private_host("[:::]"));
        assert!(is_private_host("[:::1]"));
        assert!(is_private_host("[not:an:ip]"));
        // Valid public should not be private
        assert!(!is_private_host("[2606:4700:4700::1111]"));
    }

    #[test]
    fn ipv4_mapped_consistency() {
        // Rust and Kotlin must agree: mapped private stays private
        for (mapped, inner) in [
            ("::ffff:192.168.1.1", "192.168.1.1"),
            ("::ffff:8.8.8.8", "8.8.8.8"),
        ] {
            let mapped_ip: std::net::IpAddr = mapped.parse().unwrap();
            let inner_ip: std::net::IpAddr = inner.parse().unwrap();
            assert_eq!(
                is_public_ip(mapped_ip),
                is_public_ip(inner_ip),
                "mapped {mapped} vs inner {inner}"
            );
        }
        // Compatible ::7f00:1 should delegate to 127.0.0.1 (blocked)
        let compat: std::net::IpAddr = "::7f00:1".parse().unwrap();
        let inner: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(is_public_ip(compat), is_public_ip(inner));
    }

    // -----------------------------------------------------------------------
    // Shared policy vectors (AH-14)
    // -----------------------------------------------------------------------

    /// The SAME fixture the Android unit test loads
    /// (`SiteNavigationPolicyTest.shared vectors agree with the rust policy`,
    /// copied into `app/src/test/resources/` by CI). Neither side keeps its own
    /// list of expected answers, so a divergence between `navigation_rejection`
    /// / `is_private_host` / `is_public_ip` and `SiteNavigationPolicy` /
    /// `IpPolicy` fails one of the two suites instead of shipping.
    #[test]
    fn navigation_policy_vectors_match_the_android_policy() {
        let fixture = include_str!("../../../tests/fixtures/navigation-policy.tsv");
        let (mut urls, mut hosts, mut ips) = (0usize, 0usize, 0usize);

        for raw in fixture.lines() {
            let line = raw.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            match cols[0] {
                "url" => {
                    let target = cols[1];
                    let url = Url::parse(target)
                        .unwrap_or_else(|e| panic!("{target} ayrıştırılamadı: {e}"));
                    let got = navigation_rejection(&url);
                    let want_allowed = cols[2] == "allow";
                    let want_reason = if want_allowed { None } else { Some(cols[3]) };
                    assert_eq!(got.is_none(), want_allowed, "{target}");
                    assert_eq!(got.map(NavBlock::code), want_reason, "{target}");
                    urls += 1;
                }
                "host" => {
                    let want_private = cols[2] == "private";
                    assert_eq!(is_private_host(cols[1]), want_private, "host '{}'", cols[1]);
                    hosts += 1;
                }
                "ip" => {
                    let ip: std::net::IpAddr = cols[1]
                        .parse()
                        .unwrap_or_else(|e| panic!("{} ayrıştırılamadı: {e}", cols[1]));
                    let want_public = cols[2] == "public";
                    assert_eq!(is_public_ip(ip), want_public, "ip {}", cols[1]);
                    ips += 1;
                }
                other => panic!("bilinmeyen vektör türü: {other} ({line})"),
            }
        }

        // A truncated fixture must not pass by checking nothing.
        assert!(urls >= 30, "url vektörleri kısalmış: {urls}");
        assert!(hosts >= 30, "host vektörleri kısalmış: {hosts}");
        assert!(ips >= 60, "ip vektörleri kısalmış: {ips}");
    }

    #[test]
    fn navigation_refuses_top_level_data_and_narrows_about() {
        // A top-level data: document is a fake page with an opaque origin; the
        // app's own data:image/… logos are sub-resources and never reach this
        // checkpoint, so refusing it here costs nothing.
        let data = Url::parse("data:text/html,<script>alert(1)</script>").unwrap();
        assert_eq!(navigation_rejection(&data), Some(NavBlock::DataNavigation));
        assert!(!navigation_allowed(&data));

        // about:blank is the only about: page a player or popup needs.
        assert_eq!(
            navigation_rejection(&Url::parse("about:blank").unwrap()),
            None
        );
        assert_eq!(
            navigation_rejection(&Url::parse("about:srcdoc").unwrap()),
            Some(NavBlock::UnknownAbout)
        );

        // blob: stays allowed — MSE players navigate the main frame to blob
        // object URLs, and blocking that breaks playback. A blob created over
        // plain http is a downgrade in disguise.
        assert!(navigation_allowed(
            &Url::parse("blob:https://openani.me/8c1f0a2e").unwrap()
        ));
        assert_eq!(
            navigation_rejection(&Url::parse("blob:http://openani.me/8c1f0a2e").unwrap()),
            Some(NavBlock::InsecureScheme)
        );

        // `NavBlock::MissingHost` is unreachable through `Url::parse` (a special
        // scheme without a host fails to parse) and stays as defence in depth for
        // a hand-built `Url`; its code is part of the shared vector vocabulary.
        assert_eq!(NavBlock::MissingHost.code(), "missing_host");
        assert_eq!(
            navigation_rejection(&Url::parse("https://openani.me/").unwrap()),
            None
        );
    }

    #[test]
    fn numeric_hosts_follow_the_whatwg_ipv4_parser() {
        // The url crate normalises these; Android's Uri does not, so both sides
        // run the same parser and the shared vectors pin the answers.
        for (host, private) in [
            ("0x7f000001", true),
            ("2130706433", true),
            ("127.1", true),
            ("0177.0.0.1", true),
            ("010.0.0.1", false), // octal 010 == 8 -> 8.0.0.1, a public host
            ("010.1", false),
            ("example.0x7f", true),
            ("foo.123", true),
            ("a.1.2.3.4", true),
            ("1.2.3.4.5", true),
            ("0x", true),
            ("999.1.1.1", true),
            ("8.8.8.8", false),
            ("openani.me", false),
        ] {
            assert_eq!(is_private_host(host), private, "host {host}");
        }
    }

    #[test]
    fn an_empty_or_uppercase_host_fails_closed() {
        assert!(is_private_host(""));
        assert!(is_private_host("."));
        assert!(is_private_host("LOCALHOST"));
        assert!(is_private_host("Tauri.Localhost."));
        assert!(!is_private_host("OPENANI.ME"));
    }
}
