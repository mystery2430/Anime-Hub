//! Per-site session isolation.
//!
//! ## Desktop (Linux / Windows)
//! Each site gets its own WebView window backed by its own profile directory:
//! * Linux (WebKitGTK): `WebviewWindowBuilder::data_directory`,
//! * Windows (WebView2): the profile derived from that data directory,
//!
//! so cookies, localStorage, IndexedDB and cache never cross between sites.
//!
//! ## Android
//! The system WebView has **no** per-profile data directory API, so a
//! directory-per-site partition is impossible. Instead we partition by
//! *swapping the cookie jar*: on opening a site we export the previous site's
//! cookies into its encrypted blob, wipe the live jar, and restore the target
//! site's cookies. Login state (which is cookie-borne on every site in scope)
//! is therefore isolated.
//!
//! **Documented limitation:** on Android, `localStorage` / IndexedDB / cache
//! are shared between sites because the platform offers no way to partition
//! them. Sites that keep auth state in `localStorage` would leak it. This is
//! surfaced in the About screen rather than hidden.

use crate::error::AppError;
use crate::sites::blocklist::Blocklist;
use crate::sites::url_policy::navigation_allowed;
use crate::web::dns::DnsClass;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

/// A cookie in a form that can be serialised into an encrypted blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Unix seconds; `None` means a session cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<i64>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
}

impl StoredCookie {
    /// Drop cookies the browser already expired.
    pub fn is_live(&self, now_unix: i64) -> bool {
        match self.expires {
            None => true,
            Some(ts) => ts > now_unix,
        }
    }

    /// Re-render as a `Set-Cookie` header value.
    pub fn to_header(&self) -> String {
        let mut parts = vec![format!("{}={}", self.name, self.value)];
        if let Some(d) = &self.domain {
            parts.push(format!("Domain={d}"));
        }
        if let Some(p) = &self.path {
            parts.push(format!("Path={p}"));
        }
        if let Some(ts) = self.expires {
            parts.push(format!("Expires={}", http_date(ts)));
        }
        if self.secure {
            parts.push("Secure".into());
        }
        if self.http_only {
            parts.push("HttpOnly".into());
        }
        if let Some(s) = &self.same_site {
            parts.push(format!("SameSite={s}"));
        }
        parts.join("; ")
    }
}

/// RFC 7231 date for the `Expires` attribute, computed without pulling in a
/// date crate (civil-from-days algorithm, Howard Hinnant).
pub fn http_date(unix: i64) -> String {
    // Sunday-first table, with the epoch offset: 1970-01-01 (day 0) was a
    // Thursday, which is index 4 here.
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const EPOCH_WEEKDAY: i64 = 4;
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[(days + EPOCH_WEEKDAY).rem_euclid(7) as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// The per-site encrypted cookie jar.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookieJar {
    pub site_id: String,
    pub cookies: Vec<StoredCookie>,
    /// Unix seconds of the last save, for the "clear site data" UI.
    #[serde(default)]
    pub saved_at: i64,
}

impl CookieJar {
    pub fn prune_expired(&mut self, now_unix: i64) -> usize {
        let before = self.cookies.len();
        self.cookies.retain(|c| c.is_live(now_unix));
        before - self.cookies.len()
    }
}

/// Convert a live `cookie::Cookie` into a stored one.
pub fn capture(c: &cookie::Cookie<'static>) -> StoredCookie {
    StoredCookie {
        name: c.name().to_string(),
        value: c.value().to_string(),
        domain: c.domain().map(str::to_string),
        path: c.path().map(str::to_string),
        expires: c
            .expires()
            .and_then(|e| e.datetime())
            .map(|dt| dt.unix_timestamp()),
        secure: c.secure().unwrap_or(false),
        http_only: c.http_only().unwrap_or(false),
        same_site: c.same_site().map(|s| s.to_string()),
    }
}

/// Rebuild a `cookie::Cookie` for `set_cookie`.
///
/// Returns `None` for cookies whose name/value are empty after sanitisation —
/// injecting those would produce a malformed header.
pub fn restore(sc: &StoredCookie) -> Option<cookie::Cookie<'static>> {
    if sc.name.is_empty() {
        return None;
    }
    let mut builder = cookie::Cookie::build((sc.name.clone(), sc.value.clone()));
    if let Some(d) = &sc.domain {
        builder = builder.domain(d.clone());
    }
    if let Some(p) = &sc.path {
        builder = builder.path(p.clone());
    }
    if let Some(ts) = sc.expires {
        if let Ok(dt) = time::OffsetDateTime::from_unix_timestamp(ts) {
            builder = builder.expires(dt);
        }
    }
    if sc.secure {
        builder = builder.secure(true);
    }
    if sc.http_only {
        builder = builder.http_only(true);
    }
    if let Some(s) = &sc.same_site {
        // `cookie::SameSite` has no `FromStr`, so match the Display strings.
        let ss = match s.to_ascii_lowercase().as_str() {
            "strict" => Some(cookie::SameSite::Strict),
            "lax" => Some(cookie::SameSite::Lax),
            "none" => Some(cookie::SameSite::None),
            _ => None,
        };
        if let Some(ss) = ss {
            builder = builder.same_site(ss);
        }
    }
    Some(builder.build())
}

/// Outcome of the navigation guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavDecision {
    Allow,
    /// Insecure or private-address navigation: block and report.
    Block(&'static str),
    /// Matches the blocklist: block silently.
    BlockedHost,
}

/// Shown when a public-looking name resolves to a private address.
pub const DNS_REBIND_BLOCK: &str = "Alan adı özel/ağ içi bir adrese çözümlendi.";

/// Decide whether a WebView navigation may proceed.
///
/// `site_host` is the site the session was opened for; it is unused by the
/// current rules but keeps the signature honest for future same-site rules.
///
/// This is the string policy only. Production call sites that can block the
/// WebView thread pass a [`DnsClass`] from [`decide_navigation_dns`] so a
/// name that *looks* public but resolves to a router or a metadata address
/// is refused too.
pub fn decide_navigation(url: &Url, site_host: &str, blocklist: &Blocklist) -> NavDecision {
    decide_navigation_dns(url, site_host, blocklist, DnsClass::Unknown)
}

/// [`decide_navigation`] plus the result of a DNS lookup.
///
/// `DnsClass::Unknown` (timeout, NXDOMAIN, resolver error) does **not** block:
/// a DNS outage must not make every site unopenable. A successful answer that
/// contains any non-public address does block. The string policy still runs
/// first, so literal private hosts keep their existing message.
pub fn decide_navigation_dns(
    url: &Url,
    site_host: &str,
    blocklist: &Blocklist,
    dns: DnsClass,
) -> NavDecision {
    let _ = site_host;
    if !navigation_allowed(url) {
        return NavDecision::Block(match url.scheme() {
            "http" => "Güvensiz (HTTP) yönlendirme engellendi.",
            _ => "Desteklenmeyen bir adrese yönlendirme engellendi.",
        });
    }
    if let Some(h) = url.host_str() {
        // Same rule as the "add site" form. Without this a site the user
        // trusts could bounce the WebView to their own router or to a cloud
        // metadata endpoint: the redirect never re-enters `validate_site_url`.
        if crate::sites::url_policy::is_private_host(h) {
            return NavDecision::Block("Yerel/ağ içi adrese yönlendirme engellendi.");
        }
        if blocklist.is_blocked(h) {
            return NavDecision::BlockedHost;
        }
    }
    // After the string policy. A blocklisted host is already refused, so we
    // never replace that silent decision with a toast just because DNS also
    // happened to return a private address.
    if matches!(dns, DnsClass::Answered { private: true }) {
        return NavDecision::Block(DNS_REBIND_BLOCK);
    }
    NavDecision::Allow
}

/// The script injected into every site WebView.
///
/// **Design constraint:** a remote page gets *no* Tauri IPC access — Tauri
/// disables it for `WebviewUrl::External` unless you opt in via
/// `dangerousRemoteDomainIpcAccess`, which this app deliberately does not.
/// So this script is intentionally IPC-free and purely cosmetic: it cannot
/// call back into Rust, and nothing Rust does depends on it. All real
/// blocking (HTTPS enforcement, blocklist, popup denial) happens natively in
/// the navigation and new-window handlers.
///
/// Everything dynamic reaches the page through [`embed_json`], never string
/// interpolation, so a hostile site name or blocklist entry cannot inject
/// script.
pub fn build_init_script(cfg: &InjectedConfig) -> String {
    let payload = embed_json(cfg);

    // The PiP controller installs `window.__animehubPreparePip` and
    // `window.__animehubPip`. Only Android asks for it: desktop has no PiP
    // window for a site WebView.
    let pip_controller = if cfg.pip_controller {
        format!("\n{PIP_CONTROLLER_JS}")
    } else {
        String::new()
    };

    // The popup guard is *omitted* rather than disabled when off, so the
    // shipped script never contains the `window.open` override at all.
    let popup_guard = if cfg.block_popups {
        r#"
  // Neutralise attempts to open new windows from scripts. The Rust side
  // already denies these; this stops the spinner/blank-tab artefact and the
  // "popup blocked" retry loops some ad scripts fall into.
  var nativeOpen = window.open;
  window.open = function () { return null; };
  // Keep the original reachable for first-party code that feature-detects.
  Object.defineProperty(window.open, "__animehubNative", {
    value: nativeOpen,
    enumerable: false,
  });
"#
    } else {
        ""
    };

    let cosmetic = if cfg.hide_selectors.is_empty() {
        String::new()
    } else {
        r#"
  // Cosmetic hiding for known ad containers. The selector list is data, not
  // code, so a bad entry can at worst fail to match.
  var style = document.createElement("style");
  style.id = "animehub-cosmetic";
  style.textContent = CFG.hideSelectors.join(",") + "{display:none !important;}";
  (document.head || document.documentElement).appendChild(style);
"#
        .to_string()
    };

    format!(
        r#"(function () {{
  "use strict";
  if (window.__animehubInstalled) return;
  window.__animehubInstalled = true;
  var CFG = {payload};
{popup_guard}{cosmetic}{pip_controller}}})();"#
    )
}

/// Serialise a value for embedding inside an inline `<script>` block.
///
/// `serde_json` escapes quotes, backslashes and control characters, but **not**
/// `/`, U+2028 or U+2029. A `</script>` sequence inside a JSON string literal
/// would therefore terminate the script block early and let the rest of the
/// value execute as markup — so those three are escaped explicitly. The
/// replacements are valid JSON escapes, so the payload stays parseable.
pub fn embed_json<T: serde::Serialize>(value: &T) -> String {
    let json = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    // Raw strings keep the JSON escapes literal: `\\/` in the output is the
    // JSON escape for a solidus, which is what stops `</script>` appearing.
    json.replace('/', r"\/")
        .replace('\u{2028}', r"\u2028")
        .replace('\u{2029}', r"\u2029")
}

/// Configuration handed to the init script. All fields are serialised, so
/// this doubles as the anti-injection boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InjectedConfig {
    pub block_popups: bool,
    pub hide_selectors: Vec<String>,
    /// Android only: append [`PIP_CONTROLLER_JS`] to the injected script.
    ///
    /// The controller is *also* installed by the native side on demand
    /// (`evaluateJavascript` before entering PiP), because navigating the
    /// shared Android WebView to a site replaces the document and with it
    /// anything the launcher's eval installed. Keeping it in the init script
    /// as well means the site page has it from the first paint on.
    pub pip_controller: bool,
}

impl Default for InjectedConfig {
    fn default() -> Self {
        InjectedConfig {
            block_popups: true,
            hide_selectors: DEFAULT_HIDE_SELECTORS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            // Off by default: only the Android caller turns it on.
            pip_controller: false,
        }
    }
}

/// Cosmetic selectors for the most common ad slot class names. Kept short on
/// purpose — over-hiding breaks layouts on these sites.
pub const DEFAULT_HIDE_SELECTORS: &[&str] = &[
    "[id^='google_ads_iframe']",
    ".adsbygoogle",
    ".ad-banner",
    ".ad-container",
    ".ads-container",
    ".popunder",
    ".popup-overlay",
    "#ad-overlay",
];

/// Per-site profile directory name.
///
/// Derived from the site id, restricted to a filesystem-safe charset so a
/// user-chosen id can never escape the data directory.
pub fn profile_dir_name(site_id: &str) -> String {
    let clean: String = site_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(64)
        .collect();
    if clean.is_empty() {
        "site".to_string()
    } else {
        format!("site-{clean}")
    }
}

/// Which site a session belongs to, for the cookie-swap path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub site_id: String,
    pub host: String,
}

/// In-memory index of open sessions so `close_site` knows which jar to save.
#[derive(Debug, Default)]
pub struct SessionIndex {
    inner: HashMap<String, SessionRef>,
}

impl SessionIndex {
    pub fn insert(&mut self, window_label: String, r: SessionRef) {
        self.inner.insert(window_label, r);
    }

    pub fn take(&mut self, window_label: &str) -> Option<SessionRef> {
        self.inner.remove(window_label)
    }

    pub fn get(&self, window_label: &str) -> Option<&SessionRef> {
        self.inner.get(window_label)
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Errors surfaced to the user when a navigation is blocked.
pub fn block_message(d: &NavDecision) -> Option<String> {
    match d {
        NavDecision::Allow => None,
        NavDecision::Block(msg) => Some((*msg).to_string()),
        NavDecision::BlockedHost => None, // silent
    }
}

#[allow(dead_code)]
fn assert_app_error_is_send() {
    fn check<T: Send>() {}
    check::<AppError>();
}

/// WebView-side Picture-in-Picture controller.
///
/// The same file is embedded into the Android PiP controller
/// (`AnimeHubPipController.kt`, via `scripts/android_prepare.py`), so the
/// WebView-side view and the native entry path can never drift apart. It is
/// only appended to the init script when [`InjectedConfig::pip_controller`] is
/// set — desktop never enters PiP and should keep the smaller script.
///
/// The text must stay free of dollar signs and triple quotes: it is embedded
/// inside a Kotlin raw string on the Android side (see the module tests).
pub const PIP_CONTROLLER_JS: &str = include_str!("pip_controller.js");

/// Standalone overlay snippet: floating "back to launcher" button plus an
/// `Esc` route home. Installed by Rust on every finished page load via
/// `Webview::eval` (see `web/windows.rs`), because inside a child WebView the
/// init-script channel alone proved unreliable on WebView2. The guard makes
/// repeated evals cheap and idempotent, and the interval repairs the button
/// if a SPA wipes the DOM around it.
pub const OVERLAY_SNIPPET: &str = r#"(function () {
  "use strict";
  if (window.__animehubOverlayReady) return;
  window.__animehubOverlayReady = 1;

  window.__animehubGoHome = function () {
    // Pseudo-navigation intercepted natively (never leaves the app).
    document.location.assign("animehub://close-site");
  };

  window.addEventListener(
    "keydown",
    function (e) {
      if (e.key !== "Escape") return;
      var a = document.activeElement;
      var editing =
        a &&
        ((a.tagName === "INPUT" && a.type !== "checkbox" && a.type !== "radio" &&
          a.type !== "button" && a.type !== "range") ||
          a.tagName === "TEXTAREA" ||
          a.tagName === "SELECT" ||
          a.isContentEditable);
      if (editing) {
        // First Esc steps out of the field; the next one goes home.
        a.blur();
        e.preventDefault();
        e.stopPropagation();
        return;
      }
      e.preventDefault();
      e.stopPropagation();
      window.__animehubGoHome();
    },
    true,
  );

  var install = function () {
    if (document.getElementById("__animehub-back")) return;
    var b = document.createElement("button");
    b.id = "__animehub-back";
    b.type = "button";
    b.textContent = "← AnimeHub'a dön";
    b.setAttribute(
      "style",
      "position:fixed;left:14px;bottom:14px;z-index:2147483647;" +
        "padding:9px 14px;border-radius:11px;border:1px solid rgba(255,255,255,0.28);" +
        "background:rgba(13,16,25,0.88);color:#f2f4fa;" +
        "font:600 13px/1.2 system-ui,sans-serif;cursor:pointer;" +
        "box-shadow:0 6px 22px rgba(0,0,0,0.45);backdrop-filter:blur(6px);",
    );
    b.addEventListener(
      "click",
      function (e) {
        e.preventDefault();
        e.stopPropagation();
        window.__animehubGoHome();
      },
      true,
    );
    (document.body || document.documentElement).appendChild(b);
  };

  install();
  window.setInterval(install, 1000);
})();"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::dns::DnsClass;

    fn cookie(name: &str, value: &str) -> cookie::Cookie<'static> {
        cookie::Cookie::build((name.to_string(), value.to_string()))
            .domain("openani.me".to_string())
            .path("/".to_string())
            .secure(true)
            .http_only(true)
            .same_site(cookie::SameSite::Lax)
            .build()
    }

    #[test]
    fn capture_preserves_every_attribute() {
        let c = cookie("session", "abc123");
        let s = capture(&c);
        assert_eq!(s.name, "session");
        assert_eq!(s.value, "abc123");
        assert_eq!(s.domain.as_deref(), Some("openani.me"));
        assert_eq!(s.path.as_deref(), Some("/"));
        assert!(s.secure);
        assert!(s.http_only);
        assert_eq!(s.same_site.as_deref(), Some("Lax"));
        assert!(s.expires.is_none(), "no expiry => session cookie");
    }

    #[test]
    fn restore_rebuilds_the_same_cookie() {
        let s = capture(&cookie("session", "abc123"));
        let back = restore(&s).expect("restorable");
        assert_eq!(back.name(), "session");
        assert_eq!(back.value(), "abc123");
        assert_eq!(back.domain(), Some("openani.me"));
        assert_eq!(back.secure(), Some(true));
        assert_eq!(back.same_site(), Some(cookie::SameSite::Lax));
    }

    #[test]
    fn restore_rejects_empty_name() {
        let mut s = capture(&cookie("x", "y"));
        s.name = String::new();
        assert!(restore(&s).is_none());
    }

    #[test]
    fn expiry_survives_roundtrip() {
        let ts = 1_800_000_000i64;
        let dt = time::OffsetDateTime::from_unix_timestamp(ts).unwrap();
        let c = cookie::Cookie::build(("a".to_string(), "b".to_string()))
            .expires(dt)
            .build();
        let s = capture(&c);
        assert_eq!(s.expires, Some(ts));
        let back = restore(&s).unwrap();
        assert_eq!(
            back.expires()
                .and_then(|e| e.datetime())
                .map(|d| d.unix_timestamp()),
            Some(ts)
        );
    }

    #[test]
    fn http_date_matches_known_epoch() {
        // 1970-01-01T00:00:00Z was a Thursday.
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        // 2001-09-09T01:46:40Z — the classic "1 billion seconds" timestamp.
        assert_eq!(http_date(1_000_000_000), "Sun, 09 Sep 2001 01:46:40 GMT");
        // 2024-02-29 exists (leap year).
        assert_eq!(http_date(1_709_164_800), "Thu, 29 Feb 2024 00:00:00 GMT");
    }

    #[test]
    fn http_date_handles_negative_and_subsecond() {
        assert_eq!(http_date(-1), "Wed, 31 Dec 1969 23:59:59 GMT");
        assert_eq!(http_date(59), "Thu, 01 Jan 1970 00:00:59 GMT");
    }

    #[test]
    fn to_header_emits_all_attributes() {
        let s = StoredCookie {
            name: "sid".into(),
            value: "v".into(),
            domain: Some("openani.me".into()),
            path: Some("/".into()),
            expires: Some(0),
            secure: true,
            http_only: true,
            same_site: Some("Lax".into()),
        };
        assert_eq!(
            s.to_header(),
            "sid=v; Domain=openani.me; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Secure; HttpOnly; SameSite=Lax"
        );
    }

    #[test]
    fn prune_expired_drops_only_dead_cookies() {
        let mut jar = CookieJar {
            site_id: "s".into(),
            cookies: vec![
                StoredCookie {
                    name: "live".into(),
                    value: "1".into(),
                    expires: Some(2_000_000_000),
                    ..minimal()
                },
                StoredCookie {
                    name: "dead".into(),
                    value: "1".into(),
                    expires: Some(1),
                    ..minimal()
                },
                StoredCookie {
                    name: "session".into(),
                    value: "1".into(),
                    expires: None,
                    ..minimal()
                },
            ],
            saved_at: 0,
        };
        assert_eq!(jar.prune_expired(1_900_000_000), 1);
        let names: Vec<_> = jar.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["live", "session"]);
    }

    fn minimal() -> StoredCookie {
        StoredCookie {
            name: String::new(),
            value: String::new(),
            domain: None,
            path: None,
            expires: None,
            secure: false,
            http_only: false,
            same_site: None,
        }
    }

    #[test]
    fn navigation_blocks_public_name_that_resolves_private() {
        let bl = Blocklist::default();
        let url = Url::parse("https://rebind.example/latest/meta-data/").unwrap();
        assert_eq!(
            decide_navigation_dns(
                &url,
                "openani.me",
                &bl,
                DnsClass::Answered { private: true }
            ),
            NavDecision::Block(DNS_REBIND_BLOCK)
        );
        // A resolver miss must not fail closed: the string policy already
        // accepted this host, and a DNS outage is not an attack.
        assert_eq!(
            decide_navigation_dns(&url, "openani.me", &bl, DnsClass::Unknown),
            NavDecision::Allow
        );
        assert_eq!(
            decide_navigation_dns(
                &url,
                "openani.me",
                &bl,
                DnsClass::Answered { private: false }
            ),
            NavDecision::Allow
        );
    }

    #[test]
    fn dns_answer_does_not_override_a_silent_blocklist_hit() {
        let bl = Blocklist::default();
        let url = Url::parse("https://ad.doubleclick.net/x").unwrap();
        assert_eq!(
            decide_navigation_dns(
                &url,
                "openani.me",
                &bl,
                DnsClass::Answered { private: true }
            ),
            NavDecision::BlockedHost
        );
    }

    #[test]
    fn navigation_decision_blocks_http_and_listed_hosts() {
        let bl = Blocklist::default();
        assert_eq!(
            decide_navigation(
                &Url::parse("http://openani.me/").unwrap(),
                "openani.me",
                &bl
            ),
            NavDecision::Block("Güvensiz (HTTP) yönlendirme engellendi.")
        );
        assert_eq!(
            decide_navigation(
                &Url::parse("https://ad.doubleclick.net/x").unwrap(),
                "openani.me",
                &bl
            ),
            NavDecision::BlockedHost
        );
        assert_eq!(
            decide_navigation(
                &Url::parse("https://openani.me/ep/1").unwrap(),
                "openani.me",
                &bl
            ),
            NavDecision::Allow
        );
    }

    #[test]
    fn block_message_is_silent_for_blocklist_hits() {
        assert!(block_message(&NavDecision::BlockedHost).is_none());
        assert!(block_message(&NavDecision::Allow).is_none());
        assert_eq!(
            block_message(&NavDecision::Block("x")).as_deref(),
            Some("x")
        );
    }

    #[test]
    fn profile_dir_name_is_filesystem_safe() {
        assert_eq!(
            profile_dir_name("builtin-openanime"),
            "site-builtin-openanime"
        );
        assert_eq!(profile_dir_name("../../etc/passwd"), "site-etcpasswd");
        assert_eq!(profile_dir_name(""), "site");
        assert_eq!(profile_dir_name("日本語"), "site");
    }

    #[test]
    fn session_index_tracks_and_releases() {
        let mut idx = SessionIndex::default();
        assert!(idx.is_empty());
        idx.insert(
            "w1".into(),
            SessionRef {
                site_id: "s1".into(),
                host: "a.example".into(),
            },
        );
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.get("w1").unwrap().site_id, "s1");
        let taken = idx.take("w1").unwrap();
        assert_eq!(taken.host, "a.example");
        assert!(idx.take("w1").is_none());
    }

    #[test]
    fn init_script_embeds_config_as_json_not_interpolation() {
        // A hostile name must not be able to break out of the script block.
        let cfg = InjectedConfig {
            block_popups: true,
            hide_selectors: vec!["</script><script>alert(1)</script>".into()],
            pip_controller: false,
        };
        let js = build_init_script(&cfg);
        assert!(
            !js.contains("</script><script>alert(1)"),
            "raw injection leaked"
        );
        // serde_json escapes the solidus, so the closing tag cannot appear.
        assert!(
            js.contains("<\\/script>"),
            "expected escaped solidus in {js}"
        );
        assert!(js.contains("\"blockPopups\":true"));
    }

    #[test]
    fn init_script_disables_popups_by_default() {
        let js = build_init_script(&InjectedConfig::default());
        assert!(js.contains("window.open = function"));
        assert!(js.contains("adsbygoogle"));
    }

    #[test]
    fn init_script_without_popup_blocking_leaves_open_alone() {
        let js = build_init_script(&InjectedConfig {
            block_popups: false,
            hide_selectors: vec![],
            pip_controller: false,
        });
        assert!(!js.contains("window.open = function"));
    }

    #[test]
    fn overlay_snippet_routes_home_and_focuses_editing_first() {
        // The desktop overlay is installed by Rust eval on every finished
        // page load (web/windows.rs), not by the init script, so it must be
        // a complete, self-guarding snippet on its own.
        let js = OVERLAY_SNIPPET;
        assert!(js.contains("__animehubOverlayReady"));
        assert!(js.contains("__animehub-back"));
        assert!(js.contains("animehub://close-site"));
        assert!(js.contains("Escape"), "Esc routes home");
        assert!(js.contains("isContentEditable"), "first Esc blurs editing");
        assert!(js.contains("setInterval"), "SPA-proof re-install");
        // And the init script must not shadow-install a second overlay.
        let init = build_init_script(&InjectedConfig::default());
        assert!(!init.contains("__animehub-back"));
    }

    #[test]
    fn init_script_carries_the_pip_controller_only_when_asked() {
        let with_pip = build_init_script(&InjectedConfig {
            pip_controller: true,
            ..Default::default()
        });
        assert!(
            with_pip.contains("window.__animehubPreparePip = prepare"),
            "the Android init script must install the prepare entry point"
        );
        assert!(with_pip.contains("window.__animehubPip = toggle"));
        assert!(with_pip.contains("__animehubPipVersion"));

        // Desktop keeps the smaller script: nothing there calls the controller.
        let without = build_init_script(&InjectedConfig::default());
        assert!(!without.contains("__animehubPreparePip"));
        assert!(!without.contains("__animehubPipVersion"));
    }

    #[test]
    fn pip_controller_stays_embeddable_in_a_kotlin_raw_string() {
        // scripts/android_prepare.py embeds this text in
        // AnimeHubPipController.kt; a dollar sign would interpolate and triple
        // quotes would end the literal.
        assert!(PIP_CONTROLLER_JS.contains("__animehubPreparePip"));
        assert!(PIP_CONTROLLER_JS.contains("__animehubPip"));
        assert!(!PIP_CONTROLLER_JS.contains('$'));
        assert!(!PIP_CONTROLLER_JS.contains("\"\"\""));
        // No DOM moves and no iframe internals: the promise the Android side
        // relies on for a lossless restore. (The file's own comments mention
        // these APIs when stating the rule, so the scan looks for the call
        // form, and tests/pip_controller.test.js does the same over code only.)
        assert!(!PIP_CONTROLLER_JS.contains(".contentDocument"));
        assert!(!PIP_CONTROLLER_JS.contains(".contentWindow"));
        assert!(!PIP_CONTROLLER_JS.contains("appendChild(video)"));
    }

    #[test]
    fn cookie_jar_json_roundtrips() {
        let jar = CookieJar {
            site_id: "builtin-openanime".into(),
            cookies: vec![capture(&cookie("s", "v"))],
            saved_at: 12345,
        };
        let json = serde_json::to_string(&jar).unwrap();
        let back: CookieJar = serde_json::from_str(&json).unwrap();
        assert_eq!(back, jar);
    }
}
