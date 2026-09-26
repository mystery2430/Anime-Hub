//! Domain blocklist used to suppress ad/redirect hosts inside site WebViews.
//!
//! This is a *conservative* starter list, not a full filter subscription:
//! blocking too aggressively breaks player embeds on these sites. Every entry
//! is user-editable at runtime and the whole list can be disabled.

use serde::{Deserialize, Serialize};

/// Matches a host against the list.
///
/// Semantics:
/// * `example.com` blocks `example.com` **and** any subdomain,
/// * `*.example.com` blocks subdomains only (the apex stays allowed),
/// * `=example.com` blocks the apex only,
/// * `#` starts a comment, empty lines are ignored.
pub struct Blocklist {
    suffixes: Vec<String>,
    wildcard_only: Vec<String>,
    exact: Vec<String>,
    enabled: bool,
}

impl Default for Blocklist {
    fn default() -> Self {
        Self::from_rules(DEFAULT_RULES, true)
    }
}

/// Starter list. Kept short and specific to hosts that are ads/redirectors
/// rather than CDNs a player might legitimately need.
pub const DEFAULT_RULES: &str = r#"
# --- ad networks ---
doubleclick.net
googlesyndication.com
googleadservices.com
adservice.google.com
adnxs.com
adsrvr.org
adroll.com
criteo.com
criteo.net
taboola.com
outbrain.com
sharethrough.com
pubmatic.com
openx.net
rubiconproject.com
casalemedia.com
smartadserver.com
adform.net
yieldmo.com
contextweb.com
3lift.com
bidswitch.net
# --- popunders / redirectors ---
popads.net
popcash.net
popnc.com
propellerads.com
propellerclick.com
onclickads.net
onclkds.com
juicyads.com
exoclick.com
exdynsrv.com
tsyndicate.com
hilltopads.net
adcash.com
revenuehits.com
mgid.com
revcontent.com
zedo.com
# --- trackers / analytics ---
scorecardresearch.com
quantserve.com
advertising.com
mathtag.com
adsafeprotected.com
doubleverify.com
moatads.com
# --- notification-spam push services ---
onesignal.com
pushwoosh.com
gravitec.net
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlocklistState {
    pub rules: String,
    pub enabled: bool,
}

impl Default for BlocklistState {
    fn default() -> Self {
        BlocklistState {
            rules: DEFAULT_RULES.trim_start().to_string(),
            enabled: true,
        }
    }
}

impl BlocklistState {
    pub fn default_state() -> Self {
        Self::default()
    }
}

impl Blocklist {
    pub fn from_rules(rules: &str, enabled: bool) -> Self {
        let mut suffixes = Vec::new();
        let mut wildcard_only = Vec::new();
        let mut exact = Vec::new();

        for line in rules.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let entry = line.to_ascii_lowercase();
            // A bare hostname in a text file is the "and its subdomains" form.
            if let Some(rest) = entry.strip_prefix('*') {
                let rest = rest.trim_start_matches('.');
                if !rest.is_empty() {
                    wildcard_only.push(rest.to_string());
                }
            } else if let Some(rest) = entry.strip_prefix('=') {
                if !rest.is_empty() {
                    exact.push(rest.to_string());
                }
            } else {
                suffixes.push(entry);
            }
        }

        suffixes.sort();
        suffixes.dedup();
        wildcard_only.sort();
        wildcard_only.dedup();
        exact.sort();
        exact.dedup();

        Blocklist {
            suffixes,
            wildcard_only,
            exact,
            enabled,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn len(&self) -> usize {
        self.suffixes.len() + self.wildcard_only.len() + self.exact.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Should requests/navigations to `host` be blocked?
    pub fn is_blocked(&self, host: &str) -> bool {
        if !self.enabled {
            return false;
        }
        let h = normalize_host(host);
        if h.is_empty() {
            return false;
        }

        if self.exact.binary_search(&h).is_ok() {
            return true;
        }

        if self.suffixes.binary_search(&h).is_ok() {
            return true;
        }

        // Walk every ancestor label so `a.b.ad.doubleclick.net` still matches
        // a `doubleclick.net` rule, not just its immediate child.
        for parent in ancestors(&h) {
            // `suffixes` entries match the apex and every subdomain.
            if self.suffixes.binary_search(&parent).is_ok() {
                return true;
            }
            // `*.example.com` entries match subdomains only.
            if self.wildcard_only.binary_search(&parent).is_ok() {
                return true;
            }
        }

        false
    }
}

/// Lowercase, strip trailing dot, strip brackets from IPv6 literals.
fn normalize_host(host: &str) -> String {
    let h = host.trim().to_ascii_lowercase();
    let h = h.trim_end_matches('.');
    h.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(h)
        .to_string()
}

/// Every ancestor of a host, shortest-last.
///
/// `a.b.example.com` yields `b.example.com`, then `example.com`, then `com`.
/// The apex itself is not included — callers check that separately.
fn ancestors(host: &str) -> impl Iterator<Item = String> + '_ {
    let mut rest = host
        .find('.')
        .map(|i| &host[i + 1..])
        .filter(|p| !p.is_empty());
    std::iter::from_fn(move || {
        let current = rest?;
        rest = current
            .find('.')
            .map(|i| &current[i + 1..])
            .filter(|p| !p.is_empty());
        Some(current.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bl() -> Blocklist {
        Blocklist::default()
    }

    #[test]
    fn blocks_listed_apex_and_subdomain() {
        let b = bl();
        assert!(b.is_blocked("doubleclick.net"));
        assert!(b.is_blocked("ad.doubleclick.net"));
        assert!(b.is_blocked("deep.sub.ad.doubleclick.net"));
        assert!(b.is_blocked("pagead2.googlesyndication.com"));
    }

    #[test]
    fn does_not_block_unrelated_hosts() {
        let b = bl();
        for h in [
            "openani.me",
            "animecix.com",
            "anilist.co",
            "myanimelist.net",
        ] {
            assert!(!b.is_blocked(h), "{h} must stay reachable");
        }
    }

    #[test]
    fn does_not_block_lookalike_suffixes() {
        // "notdoubleclick.net" ends with the rule string but is a different
        // registrable domain and must not be caught by a naive `ends_with`.
        let b = bl();
        assert!(!b.is_blocked("notdoubleclick.net"));
        assert!(!b.is_blocked("doubleclick.net.evil.example"));
    }

    #[test]
    fn case_and_trailing_dot_insensitive() {
        let b = bl();
        assert!(b.is_blocked("AD.DoubleClick.NET."));
    }

    #[test]
    fn wildcard_rule_matches_subdomains_only() {
        let b = Blocklist::from_rules("*.example.com", true);
        assert!(b.is_blocked("ads.example.com"));
        assert!(!b.is_blocked("example.com"));
    }

    #[test]
    fn exact_rule_matches_apex_only() {
        let b = Blocklist::from_rules("=example.com", true);
        assert!(b.is_blocked("example.com"));
        assert!(!b.is_blocked("ads.example.com"));
    }

    #[test]
    fn plain_rule_matches_apex_and_subdomains() {
        let b = Blocklist::from_rules("example.com", true);
        assert!(b.is_blocked("example.com"));
        assert!(b.is_blocked("ads.example.com"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let b = Blocklist::from_rules("# comment\n\n  \nfoo.example\n", true);
        assert_eq!(b.len(), 1);
        assert!(b.is_blocked("foo.example"));
    }

    #[test]
    fn duplicates_are_deduplicated() {
        let b = Blocklist::from_rules("a.example\na.example\nA.EXAMPLE\n", true);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn disabled_list_blocks_nothing() {
        let b = Blocklist::from_rules(DEFAULT_RULES, false);
        assert!(!b.is_blocked("doubleclick.net"));
    }

    #[test]
    fn empty_host_is_never_blocked() {
        let b = bl();
        assert!(!b.is_blocked(""));
        assert!(!b.is_blocked("   "));
    }

    #[test]
    fn ipv6_literal_is_normalised() {
        let b = Blocklist::from_rules("=2001:db8::1", true);
        assert!(b.is_blocked("[2001:db8::1]"));
    }

    #[test]
    fn default_rules_parse_without_panic() {
        let b = bl();
        assert!(
            b.len() > 30,
            "expected a real starter list, got {}",
            b.len()
        );
    }
}
