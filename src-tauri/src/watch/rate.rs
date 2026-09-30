//! The three-layer speed guarantee.
//!
//! Nothing in the app may make it poll faster than the numbers in this file.
//! Three layers stack, and each one can only ever make the result *slower*:
//!
//! 1. the engine ceiling — the constants below, not adjustable from anywhere,
//! 2. the user's own preference (a slider, clamped to the ceiling),
//! 3. the descriptor's own limit (clamped to the user's).
//!
//! On top of that sit the breaker rules: a rate limit is obeyed, the pace
//! backs off exponentially, and a host that keeps pushing back is dropped for
//! a full day. A descriptor is data, so this is what stops a shared adapter
//! file from turning the app into a scraper.

use crate::error::{AppError, AppResult};
use crate::watch::descriptor::DescriptorLimits;
use serde::{Deserialize, Serialize};

/// Shortest gap between two requests to the same host. The burst phase of the
/// airing window (10–15 min) stays above this on purpose.
pub const CEILING_MIN_INTERVAL_SECS: i64 = 600;
/// Requests per hour, per host. Never configurable.
pub const CEILING_REQUESTS_PER_HOUR: u32 = 6;
/// Requests per day, per host. Never configurable.
pub const CEILING_REQUESTS_PER_DAY: u32 = 48;
/// One request per host at a time, always: the poller is a single loop and
/// this constant documents that promise where the limits are.
pub const CONCURRENCY: u32 = 1;
/// A descriptor may not ask for a gap longer than this; the site would be
/// effectively dead.
pub const MAX_INTERVAL_SECS: i64 = 30 * 86_400;
/// How long the breaker stays open after a host keeps pushing back.
pub const BREACH_PAUSE_SECS: i64 = 24 * 3_600;
const BASE_BACKOFF_SECS: i64 = 60;
const MAX_BACKOFF_SECS: i64 = 6 * 3_600;
/// Rate limits in a row before the breaker opens.
const RATE_LIMITS_BEFORE_PAUSE: u32 = 3;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

/// The effective pace for one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Speed {
    pub min_interval_secs: i64,
    pub max_per_hour: u32,
    pub max_per_day: u32,
}

impl Default for Speed {
    fn default() -> Self {
        Speed::ceiling()
    }
}

impl Speed {
    /// The engine's own limits, before any configuration.
    pub fn ceiling() -> Self {
        Speed {
            min_interval_secs: CEILING_MIN_INTERVAL_SECS,
            max_per_hour: CEILING_REQUESTS_PER_HOUR,
            max_per_day: CEILING_REQUESTS_PER_DAY,
        }
    }

    /// Layer the descriptor and the user's preference under the ceiling.
    ///
    /// Both inputs are requests to go *slower*; anything that would speed the
    /// app up is clamped away rather than rejected, so an over-eager adapter
    /// file still imports and simply gets the engine's pace.
    pub fn resolve(descriptor: Option<DescriptorLimits>, user: Option<UserLimits>) -> Self {
        let mut speed = Speed::ceiling();
        if let Some(interval) = user.and_then(|u| u.min_interval_secs) {
            speed.slow_down_interval(i64::from(interval));
        }
        if let Some(interval) = descriptor.and_then(|d| d.min_interval_secs) {
            speed.slow_down_interval(i64::from(interval));
        }
        if let Some(per_day) = user.and_then(|u| u.max_per_day) {
            speed.slow_down_per_day(per_day);
        }
        if let Some(per_day) = descriptor.and_then(|d| d.max_requests_per_day) {
            speed.slow_down_per_day(per_day);
        }
        // The hourly ceiling never moves: no configuration buys a burst.
        speed
    }

    /// Never faster than the pace already agreed; the cap keeps a site from
    /// being configured into uselessness.
    fn slow_down_interval(&mut self, secs: i64) {
        let wanted = secs.clamp(self.min_interval_secs, MAX_INTERVAL_SECS);
        self.min_interval_secs = self.min_interval_secs.max(wanted);
    }

    /// Never more requests a day than the limit already agreed.
    fn slow_down_per_day(&mut self, per_day: u32) {
        self.max_per_day = self.max_per_day.min(per_day.max(1));
    }
}

/// The user's own preference, persisted with the other settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserLimits {
    #[serde(default)]
    pub min_interval_secs: Option<u32>,
    #[serde(default)]
    pub max_per_day: Option<u32>,
}

/// Whether a request may go out right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Not yet: wait this many seconds.
    Wait { secs: i64 },
    /// The breaker is open; the host is left alone.
    Paused { secs: i64 },
}

/// Per-host pacing state, persisted so a restart cannot reset the backoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Throttle {
    pub speed: Speed,
    #[serde(default)]
    pub last_request: Option<i64>,
    /// Request timestamps of the last day, oldest first.
    #[serde(default)]
    pub history: Vec<i64>,
    /// Extra wait imposed by a backoff, if any.
    #[serde(default)]
    pub next_allowed: Option<i64>,
    /// Set while the breaker is open.
    #[serde(default)]
    pub paused_until: Option<i64>,
    #[serde(default)]
    pub backoff_level: u32,
    #[serde(default)]
    pub rate_limits_seen: u32,
}

impl Throttle {
    pub fn new(speed: Speed) -> Self {
        Throttle {
            speed,
            last_request: None,
            history: Vec::new(),
            next_allowed: None,
            paused_until: None,
            backoff_level: 0,
            rate_limits_seen: 0,
        }
    }

    /// Seconds until a request may go out; `0` means now.
    pub fn retry_in(&self, now: i64) -> i64 {
        match self.decide(now) {
            Decision::Allow => 0,
            Decision::Wait { secs } | Decision::Paused { secs } => secs,
        }
    }

    pub fn decide(&self, now: i64) -> Decision {
        if let Some(until) = self.paused_until {
            if now < until {
                return Decision::Paused { secs: until - now };
            }
        }

        let mut wait = 0;
        if let Some(next) = self.next_allowed {
            wait = wait.max(next - now);
        }
        if let Some(last) = self.last_request {
            wait = wait.max(last + self.speed.min_interval_secs - now);
        }
        wait = wait.max(self.window_wait(now, HOUR, self.speed.max_per_hour));
        wait = wait.max(self.window_wait(now, DAY, self.speed.max_per_day));

        if wait > 0 {
            Decision::Wait { secs: wait }
        } else {
            Decision::Allow
        }
    }

    /// Take a slot. Refuses — never silently trims — when the pace would be
    /// broken, so a caller that ignores [`Throttle::decide`] gets an error
    /// instead of a burst.
    pub fn record_request(&mut self, now: i64) -> AppResult<()> {
        match self.decide(now) {
            Decision::Allow => {
                self.last_request = Some(now);
                self.history.push(now);
                self.prune_history(now);
                Ok(())
            }
            Decision::Wait { secs } => Err(AppError::Watch(format!(
                "istek çok erken; {secs} sn sonra"
            ))),
            Decision::Paused { .. } => {
                // Asking while the breaker is open is itself a breach: the
                // pause is renewed rather than shortened.
                self.paused_until = Some(now + BREACH_PAUSE_SECS);
                Err(AppError::Watch("devre kesici açık".into()))
            }
        }
    }

    /// A healthy response clears the backoff.
    pub fn record_ok(&mut self, now: i64) {
        self.backoff_level = 0;
        self.rate_limits_seen = 0;
        self.next_allowed = None;
        self.prune_history(now);
    }

    /// A `429` (or a `Retry-After` we must honour).
    pub fn record_rate_limited(&mut self, now: i64, retry_after: Option<i64>) {
        self.rate_limits_seen = self.rate_limits_seen.saturating_add(1);
        self.backoff_level = self.backoff_level.saturating_add(1);
        let backoff = self.backoff_secs();
        let wait = retry_after.unwrap_or(0).max(backoff).min(MAX_BACKOFF_SECS);
        self.next_allowed = Some(now + wait);
        if self.rate_limits_seen >= RATE_LIMITS_BEFORE_PAUSE {
            self.paused_until = Some(now + BREACH_PAUSE_SECS);
        }
    }

    /// A `5xx`: back off, but do not treat a broken server as a pushback.
    pub fn record_server_error(&mut self, now: i64) {
        self.backoff_level = self.backoff_level.saturating_add(1);
        self.next_allowed = Some(now + self.backoff_secs());
    }

    fn backoff_secs(&self) -> i64 {
        let exponent = self.backoff_level.saturating_sub(1).min(6);
        BASE_BACKOFF_SECS
            .saturating_mul(1_i64 << exponent)
            .min(MAX_BACKOFF_SECS)
    }

    /// Wait imposed by a rolling window: the slot frees when the oldest
    /// request that is still holding one ages out.
    fn window_wait(&self, now: i64, window: i64, limit: u32) -> i64 {
        if limit == 0 {
            return window;
        }
        let recent: Vec<i64> = self
            .history
            .iter()
            .copied()
            .filter(|stamp| *stamp > now - window)
            .collect();
        let limit = limit as usize;
        if recent.len() < limit {
            return 0;
        }
        let oldest_holding = recent[recent.len() - limit];
        (oldest_holding + window - now).max(0)
    }

    fn prune_history(&mut self, now: i64) {
        let cutoff = now - DAY;
        self.history.retain(|stamp| *stamp > cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    fn speed(min_interval_secs: i64, max_per_hour: u32, max_per_day: u32) -> Speed {
        Speed {
            min_interval_secs,
            max_per_hour,
            max_per_day,
        }
    }

    #[test]
    fn a_descriptor_can_only_slow_the_app_down() {
        // Asking for a 30 s gap and 10 000 requests a day gets the engine's
        // numbers instead of a rejection.
        let greedy = DescriptorLimits {
            min_interval_secs: Some(30),
            max_requests_per_day: Some(10_000),
        };
        let resolved = Speed::resolve(Some(greedy), None);
        assert_eq!(resolved, Speed::ceiling());

        // Asking for a slower pace is honoured.
        let polite = DescriptorLimits {
            min_interval_secs: Some(7_200),
            max_requests_per_day: Some(4),
        };
        let resolved = Speed::resolve(Some(polite), None);
        assert_eq!(resolved.min_interval_secs, 7_200);
        assert_eq!(resolved.max_per_day, 4);
        assert_eq!(resolved.max_per_hour, CEILING_REQUESTS_PER_HOUR);
    }

    #[test]
    fn the_user_layer_stacks_under_the_descriptor() {
        let user = UserLimits {
            min_interval_secs: Some(3_600),
            max_per_day: Some(8),
        };
        let descriptor = DescriptorLimits {
            min_interval_secs: Some(1_800),
            max_requests_per_day: Some(20),
        };
        let resolved = Speed::resolve(Some(descriptor), Some(user));
        assert_eq!(resolved.min_interval_secs, 3_600, "en yavaş katman kazanır");
        assert_eq!(resolved.max_per_day, 8);

        // A user asking for an absurd interval is clamped, not obeyed.
        let absurd = UserLimits {
            min_interval_secs: Some(u32::MAX),
            max_per_day: None,
        };
        let clamped = Speed::resolve(None, Some(absurd));
        assert_eq!(clamped.min_interval_secs, MAX_INTERVAL_SECS);
    }

    #[test]
    fn the_first_request_is_allowed_and_the_next_one_waits() {
        let mut throttle = Throttle::new(Speed::ceiling());
        assert_eq!(throttle.decide(NOW), Decision::Allow);
        throttle.record_request(NOW).expect("ilk istek");

        assert_eq!(
            throttle.decide(NOW + 1),
            Decision::Wait {
                secs: CEILING_MIN_INTERVAL_SECS - 1,
            }
        );
        let early = throttle.record_request(NOW + 1);
        assert!(early.is_err(), "erken istek reddedilir");
        let later = throttle.decide(NOW + CEILING_MIN_INTERVAL_SECS);
        assert_eq!(later, Decision::Allow);
    }

    #[test]
    fn the_hourly_ceiling_holds_a_burst() {
        // A zero interval isolates the window rule from the spacing rule.
        let mut throttle = Throttle::new(speed(0, 2, 48));
        throttle.record_request(NOW - 100).expect("1");
        throttle.record_request(NOW - 50).expect("2");

        assert_eq!(throttle.decide(NOW), Decision::Wait { secs: HOUR - 100 });
        // Once the oldest of the two ages out, the slot frees again.
        assert_eq!(throttle.decide(NOW + HOUR - 50), Decision::Allow);
    }

    #[test]
    fn the_daily_ceiling_is_the_last_word() {
        let mut throttle = Throttle::new(speed(0, 100, 2));
        throttle.record_request(NOW - 2 * HOUR).expect("1");
        throttle.record_request(NOW - HOUR).expect("2");

        assert_eq!(
            throttle.decide(NOW),
            Decision::Wait { secs: DAY - 2 * HOUR }
        );
    }

    #[test]
    fn rate_limits_back_off_and_then_open_the_breaker() {
        let mut throttle = Throttle::new(Speed::ceiling());

        throttle.record_rate_limited(NOW, None);
        assert_eq!(throttle.decide(NOW), Decision::Wait { secs: 60 });
        assert_eq!(throttle.decide(NOW + 60), Decision::Allow);

        throttle.record_rate_limited(NOW + 60, None);
        assert_eq!(throttle.decide(NOW + 60), Decision::Wait { secs: 120 });

        // The third pushback in a row drops the host for a day.
        throttle.record_rate_limited(NOW + 200, None);
        match throttle.decide(NOW + 201) {
            Decision::Paused { secs } => assert_eq!(secs, BREACH_PAUSE_SECS - 1),
            other => panic!("devre kesici bekleniyordu: {other:?}"),
        }
        assert!(throttle.record_request(NOW + 202).is_err());
        // Asking while paused renews the pause instead of eroding it.
        let renewed = BREACH_PAUSE_SECS + 202 - 10_000;
        assert_eq!(
            throttle.decide(NOW + 10_000),
            Decision::Paused { secs: renewed }
        );
    }

    #[test]
    fn retry_after_is_honoured_without_opening_the_breaker() {
        let mut throttle = Throttle::new(Speed::ceiling());
        throttle.record_rate_limited(NOW, Some(3_600));
        assert_eq!(throttle.decide(NOW), Decision::Wait { secs: 3_600 });
        assert_eq!(throttle.decide(NOW + 3_600), Decision::Allow);
        assert_eq!(throttle.rate_limits_seen, 1);
        assert_eq!(throttle.paused_until, None);
    }

    #[test]
    fn an_absurd_retry_after_is_capped() {
        let mut throttle = Throttle::new(Speed::ceiling());
        throttle.record_rate_limited(NOW, Some(365 * DAY));
        assert_eq!(throttle.retry_in(NOW), MAX_BACKOFF_SECS);
    }

    #[test]
    fn a_healthy_response_clears_the_backoff() {
        let mut throttle = Throttle::new(Speed::ceiling());
        throttle.record_rate_limited(NOW, None);
        throttle.record_server_error(NOW + 60);
        assert!(throttle.retry_in(NOW + 60) > 0);

        throttle.record_ok(NOW + 5_000);
        assert_eq!(throttle.backoff_level, 0);
        assert_eq!(throttle.rate_limits_seen, 0);
        let later = NOW + 5_000 + CEILING_MIN_INTERVAL_SECS;
        assert_eq!(throttle.decide(later), Decision::Allow);
    }

    #[test]
    fn history_stays_bounded() {
        let mut throttle = Throttle::new(speed(0, 100, 1000));
        for step in 0..10 {
            let now = NOW + i64::from(step) * HOUR;
            throttle.record_request(now).expect("istek");
            throttle.record_ok(now);
        }
        assert!(throttle.history.len() <= 25, "tarihçe bir günle sınırlı");
    }
}
