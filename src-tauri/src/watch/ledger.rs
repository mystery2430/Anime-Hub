//! The seen ledger: what the app has already told the user about.
//!
//! A notification is only ever sent on the *transition* from unseen to seen,
//! so a reinstall, a restart or a re-added site can never replay yesterday's
//! episodes. The first poll of a site is a silent baseline: it records what
//! the site is currently listing and announces nothing, because "everything
//! on this page is new to us" is not news.
//!
//! The ledger is keyed by site, title, season and episode. Titles reaching
//! this module are already canonical ([`crate::watch::normalize`]), so two
//! spellings of the same release collapse into one entry.

use crate::watch::normalize::Release;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Hard cap on remembered episodes. Beyond this the oldest are dropped.
pub const MAX_ENTRIES: usize = 4096;
/// Episodes not seen for this long are forgotten.
pub const RETENTION_DAYS: i64 = 120;
/// A turn that surfaces this many fresh episodes is summarised in one
/// notification instead of one per episode.
pub const DIGEST_THRESHOLD: usize = 3;

/// One remembered episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeenEntry {
    pub site_id: String,
    pub title: String,
    pub season: u32,
    pub episode: u32,
    /// Unix seconds of the first time the app saw this episode.
    pub first_seen: i64,
    pub last_seen: i64,
    /// `true` once the user has been told (or once it was part of a silent
    /// baseline).
    pub announced: bool,
}

/// What one call to [`Ledger::observe`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    /// The ledger had never seen this episode.
    Fresh,
    /// Already known.
    Known,
}

/// What to deliver after one poll of one site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    /// Nothing fresh.
    Quiet,
    /// The site had no baseline yet: everything was recorded, nothing is
    /// announced.
    Baseline { recorded: usize },
    /// Notifications to deliver, in order.
    Announce(Vec<Notice>),
}

/// One notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A single new episode.
    Single(Release),
    /// Several new episodes at once, delivered as one summary.
    Digest { count: usize, episode: Release },
}

/// The ledger document, persisted encrypted with the other app state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    #[serde(default)]
    pub version: u32,
    /// Sites whose silent baseline has been recorded.
    #[serde(default)]
    pub baselines: BTreeSet<String>,
    #[serde(default)]
    pub entries: BTreeMap<String, SeenEntry>,
}

/// Stable key for one episode of one site. The unit separator cannot appear
/// in a title, so the parts can never run into each other.
pub fn key(site_id: &str, title: &str, season: u32, episode: u32) -> String {
    format!("{site_id}\u{1f}{title}\u{1f}{season}\u{1f}{episode}")
}

impl Ledger {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record one episode and report whether it was new.
    pub fn observe(&mut self, site_id: &str, release: &Release, now: i64) -> Observation {
        let key = key(site_id, &release.title, release.season, release.episode);
        match self.entries.get_mut(&key) {
            Some(entry) => {
                entry.last_seen = now;
                Observation::Known
            }
            None => {
                self.entries.insert(
                    key,
                    SeenEntry {
                        site_id: site_id.to_string(),
                        title: release.title.clone(),
                        season: release.season,
                        episode: release.episode,
                        first_seen: now,
                        last_seen: now,
                        announced: false,
                    },
                );
                Observation::Fresh
            }
        }
    }

    pub fn has_baseline(&self, site_id: &str) -> bool {
        self.baselines.contains(site_id)
    }

    /// Record everything a site currently lists as already seen, without
    /// announcing any of it.
    pub fn seed(&mut self, site_id: &str, releases: &[Release], now: i64) -> usize {
        self.baselines.insert(site_id.to_string());
        for release in releases {
            let key = key(site_id, &release.title, release.season, release.episode);
            let entry = self.entries.entry(key).or_insert_with(|| SeenEntry {
                site_id: site_id.to_string(),
                title: release.title.clone(),
                season: release.season,
                episode: release.episode,
                first_seen: now,
                last_seen: now,
                announced: true,
            });
            entry.last_seen = now;
            entry.announced = true;
        }
        releases.len()
    }

    /// Decide what one poll of one site should deliver.
    ///
    /// `fresh` is what the caller just read, not a pre-filtered list: anything
    /// already announced is dropped here, so re-reading the same listing can
    /// never notify twice. Keeping that transition in one place — rather than
    /// at every call site — is what makes the single-notification promise
    /// hold even when a caller is careless.
    ///
    /// The first call for a site always answers [`Turn::Baseline`].
    pub fn plan(&mut self, site_id: &str, fresh: &[Release], now: i64) -> Turn {
        if fresh.is_empty() {
            return Turn::Quiet;
        }
        if !self.has_baseline(site_id) {
            let recorded = self.seed(site_id, fresh, now);
            return Turn::Baseline { recorded };
        }

        let mut unseen: Vec<Release> = Vec::new();
        for release in fresh {
            // Recording here also refreshes `last_seen`, so an episode that
            // stays in the listing does not age out of the ledger.
            self.observe(site_id, release, now);
            let key = key(site_id, &release.title, release.season, release.episode);
            let announced = self
                .entries
                .get(&key)
                .is_some_and(|entry| entry.announced);
            if !announced {
                unseen.push(release.clone());
            }
        }

        if unseen.is_empty() {
            return Turn::Quiet;
        }
        for release in &unseen {
            self.mark_announced(site_id, release, now);
        }
        if unseen.len() >= DIGEST_THRESHOLD {
            Turn::Announce(vec![Notice::Digest {
                count: unseen.len(),
                episode: unseen[0].clone(),
            }])
        } else {
            Turn::Announce(unseen.iter().cloned().map(Notice::Single).collect())
        }
    }

    /// Forget a site: its entries and its baseline. Removing a site from the
    /// launcher must also forget what was announced for it — but re-adding it
    /// starts with a fresh silent baseline, so nothing is replayed.
    pub fn forget_site(&mut self, site_id: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| entry.site_id != site_id);
        self.baselines.remove(site_id);
        before - self.entries.len()
    }

    /// Drop entries that have aged out, then the oldest until the ledger fits.
    /// Returns how many entries were removed.
    pub fn prune(&mut self, now: i64) -> usize {
        let before = self.entries.len();
        let cutoff = now - RETENTION_DAYS * 86_400;
        self.entries.retain(|_, entry| entry.last_seen >= cutoff);

        if self.entries.len() > MAX_ENTRIES {
            let mut by_age: Vec<(i64, String)> = self
                .entries
                .iter()
                .map(|(key, entry)| (entry.last_seen, key.clone()))
                .collect();
            // Oldest first; the key breaks ties so pruning is deterministic.
            by_age.sort();
            let excess = self.entries.len() - MAX_ENTRIES;
            for (_, key) in by_age.into_iter().take(excess) {
                self.entries.remove(&key);
            }
        }
        before - self.entries.len()
    }

    fn mark_announced(&mut self, site_id: &str, release: &Release, now: i64) {
        let key = key(site_id, &release.title, release.season, release.episode);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.announced = true;
            entry.last_seen = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(title: &str, episode: u32) -> Release {
        Release {
            title: title.into(),
            season: 1,
            episode,
        }
    }

    const SITE: &str = "site-a";
    const NOW: i64 = 1_700_000_000;

    #[test]
    fn the_first_poll_of_a_site_is_silent() {
        let mut ledger = Ledger::default();
        let listed = vec![release("show a", 10), release("show a", 11)];

        assert_eq!(
            ledger.plan(SITE, &listed, NOW),
            Turn::Baseline { recorded: 2 }
        );
        assert!(ledger.has_baseline(SITE));
        assert_eq!(ledger.len(), 2);
        assert!(ledger.entries.values().all(|e| e.announced));

        // The same list on the next poll says nothing.
        assert_eq!(ledger.plan(SITE, &listed, NOW + 600), Turn::Quiet);
    }

    #[test]
    fn only_the_unseen_to_seen_transition_notifies() {
        let mut ledger = Ledger::default();
        ledger.plan(SITE, &[release("show a", 10)], NOW);

        let fresh = vec![release("show a", 11)];
        assert_eq!(
            ledger.plan(SITE, &fresh, NOW + 600),
            Turn::Announce(vec![Notice::Single(release("show a", 11))])
        );
        // Repeating it is not news.
        assert_eq!(ledger.plan(SITE, &fresh, NOW + 1200), Turn::Quiet);
    }

    #[test]
    fn a_burst_collapses_into_one_summary() {
        let mut ledger = Ledger::default();
        ledger.plan(SITE, &[release("show a", 1)], NOW);

        let burst = vec![
            release("show a", 2),
            release("show a", 3),
            release("show a", 4),
        ];
        let turn = ledger.plan(SITE, &burst, NOW + 600);
        match turn {
            Turn::Announce(notices) => {
                assert_eq!(notices.len(), 1, "üç yeni bölüm tek bildirim olur");
                assert_eq!(
                    notices[0],
                    Notice::Digest {
                        count: 3,
                        episode: release("show a", 2),
                    }
                );
            }
            other => panic!("özet bekleniyordu: {other:?}"),
        }
    }

    #[test]
    fn two_fresh_episodes_are_announced_separately() {
        let mut ledger = Ledger::default();
        ledger.plan(SITE, &[release("show a", 1)], NOW);
        let turn = ledger.plan(
            SITE,
            &[release("show a", 2), release("show a", 3)],
            NOW + 600,
        );
        match turn {
            Turn::Announce(notices) => assert_eq!(notices.len(), 2),
            other => panic!("iki bildirim bekleniyordu: {other:?}"),
        }
    }

    #[test]
    fn a_reinstall_starts_from_a_new_silent_baseline() {
        // A fresh ledger has no memory, so the next poll is a baseline — the
        // user is not spammed with the whole back catalogue.
        let mut reinstall = Ledger::default();
        let listed: Vec<Release> = (1..=20).map(|n| release("show a", n)).collect();
        assert_eq!(
            reinstall.plan(SITE, &listed, NOW),
            Turn::Baseline { recorded: 20 }
        );
    }

    #[test]
    fn observing_twice_keeps_the_first_sighting() {
        let mut ledger = Ledger::default();
        assert_eq!(
            ledger.observe(SITE, &release("show a", 5), NOW),
            Observation::Fresh
        );
        assert_eq!(
            ledger.observe(SITE, &release("show a", 5), NOW + 300),
            Observation::Known
        );
        let key = key(SITE, "show a", 1, 5);
        let entry = ledger.entries.get(&key).expect("kayıt");
        assert_eq!(entry.first_seen, NOW);
        assert_eq!(entry.last_seen, NOW + 300);
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn seasons_and_titles_are_part_of_the_key() {
        let mut ledger = Ledger::default();
        ledger.observe(SITE, &release("show a", 5), NOW);
        let other_season = Release {
            title: "show a".into(),
            season: 2,
            episode: 5,
        };
        assert_eq!(ledger.observe(SITE, &other_season, NOW), Observation::Fresh);
        assert_eq!(
            ledger.observe("site-b", &release("show a", 5), NOW),
            Observation::Fresh
        );
    }

    #[test]
    fn forgetting_a_site_clears_its_memory() {
        let mut ledger = Ledger::default();
        ledger.plan(SITE, &[release("show a", 1)], NOW);
        ledger.plan("site-b", &[release("show b", 1)], NOW);

        assert_eq!(ledger.forget_site(SITE), 1);
        assert!(!ledger.has_baseline(SITE));
        assert_eq!(ledger.len(), 1);
        // Re-adding the site baselines again instead of announcing.
        assert_eq!(
            ledger.plan(
                SITE,
                &[release("show a", 1), release("show a", 2)],
                NOW + 60
            ),
            Turn::Baseline { recorded: 2 }
        );
    }

    #[test]
    fn pruning_ages_out_and_then_caps() {
        let mut ledger = Ledger::default();
        let old = NOW - (RETENTION_DAYS + 1) * 86_400;
        ledger.observe(SITE, &release("ancient", 1), old);
        ledger.observe(SITE, &release("recent", 1), NOW);
        assert_eq!(ledger.prune(NOW), 1);
        assert_eq!(ledger.len(), 1);

        let mut full = Ledger::default();
        for n in 0..(MAX_ENTRIES as u32 + 10) {
            let release = release("bulk", n + 1);
            full.observe(SITE, &release, NOW + i64::from(n));
        }
        assert_eq!(full.len(), MAX_ENTRIES + 10);
        assert_eq!(full.prune(NOW + 100_000), 10);
        assert_eq!(full.len(), MAX_ENTRIES);
        // The oldest are the ones that went.
        assert!(full.entries.contains_key(&key(SITE, "bulk", 1, 11)));
        assert!(!full.entries.contains_key(&key(SITE, "bulk", 1, 1)));
    }
}
