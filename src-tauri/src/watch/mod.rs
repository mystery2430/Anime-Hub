//! New-episode notifications (v0.4.0).
//!
//! The engine is deliberately split from anything that touches the network:
//!
//! * [`descriptor`] — the declarative site adapter (schema v1) and the JSON
//!   pointer / field extraction on top of it,
//! * [`normalize`] — one canonical `(title, season, episode)` for every site
//!   spelling,
//! * [`ledger`] — what has already been announced, so a notification is sent
//!   exactly once,
//! * [`rate`] — the three-layer speed guarantee and the circuit breaker.
//!
//! None of these modules perform I/O, which is what makes the whole surface
//! testable without a network and keeps the pacing rules in one auditable
//! place. Fetching, the AniList airing calendar and the notification surface
//! are layered on top of them.
//!
//! Two rules hold everywhere in this tree:
//!
//! * a descriptor is data — it can ask for *less* traffic, never more, and it
//!   can never borrow the user's session (no cookies, no non-GET methods),
//! * a title is only turned into a notification after it has been matched to
//!   an AniList entry; an unmatched row is recorded but stays silent.

pub mod descriptor;
pub mod ledger;
pub mod normalize;
pub mod rate;

use serde::{Deserialize, Serialize};

/// Encrypted document holding the descriptors and the user's pace settings.
pub const CONFIG_DOC: &str = "watch.bin";
/// Encrypted document holding the seen ledger.
pub const LEDGER_DOC: &str = "watch-ledger.bin";

/// Everything the notification engine persists, except the ledger.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchConfig {
    #[serde(default)]
    pub version: u32,
    /// Off until the user turns it on.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub descriptors: Vec<descriptor::Descriptor>,
    /// The user's own pace preference, applied under the engine ceiling.
    #[serde(default)]
    pub limits: rate::UserLimits,
}

impl WatchConfig {
    /// The descriptor responsible for a host, if the user imported one.
    pub fn descriptor_for(&self, host: &str) -> Option<&descriptor::Descriptor> {
        self.descriptors.iter().find(|d| d.host == host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_inert() {
        let config = WatchConfig::default();
        assert!(!config.enabled, "bildirim motoru kendiliğinden açılmaz");
        assert!(config.descriptors.is_empty());
        assert!(config.descriptor_for("example-anime.test").is_none());
    }

    #[test]
    fn config_round_trips_through_json() {
        let config = WatchConfig {
            version: 1,
            enabled: true,
            descriptors: Vec::new(),
            limits: rate::UserLimits {
                min_interval_secs: Some(3_600),
                max_per_day: Some(12),
            },
        };
        let json = serde_json::to_string(&config).expect("serialise");
        let back: WatchConfig = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(config, back);
    }
}
