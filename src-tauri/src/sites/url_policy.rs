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

/// Guard for in-WebView navigation.
///
/// Stricter than [`validate_site_url`]: an `http` navigation is *denied*
/// rather than reported as "not configured", and non-http schemes used by
/// media players (`blob:` for MSE video, `data:` for tiny inline assets) are
/// allowed through because blocking them breaks legitimate playback. The
/// `blob`/`data` allowance is deliberate and scoped to navigation, never to
/// stored site URLs.
pub fn navigation_allowed(url: &Url) -> bool {
    match url.scheme() {
        "https" => {}
        // Downgrades are blocked, not silently upgraded.
        "http" => return false,
        // Playback/inline-asset schemes are permitted inside a session that
        // already started on HTTPS.
        "blob" | "data" | "about" => return true,
        _ => return false,
    }

    match url.host_str() {
        None => true, // blob:/data: have no host
        Some(h) => {
            let h = h.to_lowercase();
            !(h == "localhost" || is_private_host(&h))
        }
    }
}

/// True for loopback, private, link-local and other non-routable hosts.
///
/// Public because the in-WebView navigation guard needs the same rule:
/// [`validate_site_url`] protects the *stored* entry point, but a site is free
/// to redirect the WebView to `https://192.168.1.1/` afterwards, and that
/// navigation never passes through `validate_site_url` again.
pub fn is_private_host(host: &str) -> bool {
    let h = host.trim_end_matches('.');

    // Literal IPv6 in `url` keeps the brackets.
    let bare = h
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(h);
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return !ip_is_public(&ip);
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

fn ip_is_public(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                // 100.64.0.0/10 CGNAT
                || (a == 100 && (64..=127).contains(&b))
                // 192.0.0.0/24 and 198.18.0.0/15 benchmarking
                || (a == 192 && b == 0)
                || (a == 198 && (18..=19).contains(&b)))
        }
        std::net::IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // ::ffff:0:0/96 mapped IPv4
                || v6.to_ipv4_mapped().is_some_and(|m| !ip_is_public(&m.into()))
                // fe80::/10 link-local, fc00::/7 unique-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.segments()[0] & 0xfe00) == 0xfc00)
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
        let s = validate_site_url("https://animecix.com/series/x?tab=1#frag").expect("valid");
        assert_eq!(s.as_str(), "https://animecix.com/series/x?tab=1");
        assert_eq!(s.host(), "animecix.com");
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
}
