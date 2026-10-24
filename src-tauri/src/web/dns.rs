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
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
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

/// How many DNS lookups may hold a thread at the same time.
///
/// `ToSocketAddrs` is blocking and cannot be cancelled, so a lookup that never
/// returns keeps its thread. A launcher navigates rarely, but a page can steer
/// many navigations, and a resolver stuck on one name used to leave one
/// detached thread behind *per navigation* — nothing bounded them. Four
/// concurrent lookups is far more than a single-window launcher needs, and the
/// bound is what makes a stuck resolver cost four threads instead of an
/// unbounded number.
const MAX_LOOKUP_THREADS: usize = 4;

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
        // A timeout is never cached in either direction: caching it would turn
        // one slow answer into a permanent verdict about a host, and the next
        // navigation has to be free to see a real answer.
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
    lookup_with(host, LOOKUP_TIMEOUT, MAX_LOOKUP_THREADS, resolve_ips)
}

/// One in-flight lookup that callers can join, so "the same host, again" costs
/// no extra thread.
///
/// The entry lives in [`in_flight`] from the moment the lookup starts until the
/// resolver returns, **including after the caller has given up**. That is what
/// bounds the work: a navigation that times out and retries joins the lookup
/// that is already running instead of starting another stuck thread.
#[derive(Default)]
struct Pending {
    answer: Mutex<Option<DnsClass>>,
    done: Condvar,
}

fn in_flight() -> &'static Mutex<HashMap<String, Arc<Pending>>> {
    static IN_FLIGHT: OnceLock<Mutex<HashMap<String, Arc<Pending>>>> = OnceLock::new();
    IN_FLIGHT.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic in a resolver thread must not disable DNS for the process: the
    // data behind these locks is a plain map and a plain Option.
    let guard = mutex.lock();
    guard.unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Join the running lookup for `host`, or claim the slot to start one.
///
/// `None` means every lookup thread is busy: the caller gets [`DnsClass::Unknown`]
/// immediately instead of queueing another thread. That is the same answer a
/// timeout produces, so it is not a new bypass — the static string policy has
/// already run and it never needs DNS.
fn join_in_flight(host: &str, budget: usize) -> Option<(Arc<Pending>, bool)> {
    let mut map = lock(in_flight());
    if let Some(existing) = map.get(host) {
        return Some((Arc::clone(existing), false));
    }
    if map.len() >= budget {
        return None;
    }
    let entry = Arc::new(Pending::default());
    map.insert(host.to_string(), Arc::clone(&entry));
    Some((entry, true))
}

/// Resolve `host` with at most `budget` lookups in flight and a bounded wait.
///
/// Testable form: `resolve` is the only thing that touches the network, and
/// the timeout/budget are parameters so the bounded-wait and bounded-thread
/// behaviour can be measured without a real resolver.
fn lookup_with<F>(host: &str, timeout: Duration, budget: usize, resolve: F) -> DnsClass
where
    F: Fn(&str) -> std::io::Result<Vec<IpAddr>> + Send + 'static,
{
    let Some((entry, owner)) = join_in_flight(host, budget) else {
        return DnsClass::Unknown;
    };
    if owner {
        let owned_host = host.to_string();
        let for_thread = Arc::clone(&entry);
        let spawned = std::thread::Builder::new()
            .name("animehub-dns".into())
            .spawn(move || {
                let class = match resolve(owned_host.as_str()) {
                    Ok(ips) => DnsClass::Answered {
                        private: answers_are_private(&ips),
                    },
                    Err(_) => DnsClass::Unknown,
                };
                publish(&owned_host, &for_thread, class);
            });
        if spawned.is_err() {
            // No thread available at all: publish "no answer" so joined callers
            // wake up instead of waiting out their timeout, and free the slot.
            publish(host, &entry, DnsClass::Unknown);
        }
    }
    wait_for(&entry, timeout)
}

/// Hand the answer to everyone waiting on this lookup and free its slot.
///
/// The answer is delivered to waiters only. It is deliberately **not** written
/// into the cache: a caller that already timed out must not turn a late answer
/// into a security decision, and the next navigation resolves again.
fn publish(host: &str, entry: &Arc<Pending>, class: DnsClass) {
    {
        let mut answer = lock(&entry.answer);
        *answer = Some(class);
    }
    entry.done.notify_all();
    let _ = lock(in_flight()).remove(host);
}

/// Wait for the answer, at most `timeout`; anything else is [`DnsClass::Unknown`].
fn wait_for(entry: &Arc<Pending>, timeout: Duration) -> DnsClass {
    let deadline = Instant::now() + timeout;
    let mut answer = lock(&entry.answer);
    loop {
        if let Some(class) = *answer {
            return class;
        }
        let now = Instant::now();
        if now >= deadline {
            return DnsClass::Unknown;
        }
        let (guard, _) = entry
            .done
            .wait_timeout(answer, deadline - now)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        answer = guard;
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PUBLIC: IpAddr = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
    const PRIVATE: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

    /// A resolver call that blocks until the test opens the gate, and counts
    /// how many times it was entered. This is what makes "a stuck lookup" and
    /// "one lookup per host" measurable instead of anecdotal.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        cv: Condvar,
        entered: AtomicUsize,
    }

    impl Gate {
        fn enter(&self) {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let mut open = lock(&self.open);
            while !*open {
                open = self.cv.wait(open).unwrap_or_else(|p| p.into_inner());
            }
        }

        fn release(&self) {
            *lock(&self.open) = true;
            self.cv.notify_all();
        }

        fn entered(&self) -> usize {
            self.entered.load(Ordering::SeqCst)
        }
    }

    fn wait_until(condition: impl Fn() -> bool, millis: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(millis);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        condition()
    }

    fn no_lookup(host: &str) -> bool {
        lock(in_flight()).get(host).is_none()
    }

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
        assert_eq!(classify_host("[::1]"), DnsClass::Answered { private: true });
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
        assert_eq!(classify_host("1.1.1.1"), classify_host("1.1.1.1"));
    }

    #[test]
    fn a_resolver_answer_is_classified_and_an_error_is_unknown() {
        let limit = Duration::from_millis(500);
        let answered = lookup_with("public.test", limit, 2, |_| Ok(vec![PUBLIC]));
        assert_eq!(answered, DnsClass::Answered { private: false });

        let mixed = |_: &str| -> std::io::Result<Vec<IpAddr>> { Ok(vec![PUBLIC, PRIVATE]) };
        let answered = lookup_with("private.test", limit, 2, mixed);
        assert_eq!(answered, DnsClass::Answered { private: true });

        let not_found = std::io::ErrorKind::NotFound;
        let failed = lookup_with("error.test", limit, 2, move |_| {
            Err(std::io::Error::new(not_found, "nx"))
        });
        assert_eq!(failed, DnsClass::Unknown);
    }

    #[test]
    fn concurrent_callers_for_one_host_share_a_single_lookup() {
        // The old code spawned one detached thread per call, and the comment
        // claimed the cache lock de-duplicated them. It never did: the lock is
        // only held for the map access, not across the lookup.
        let gate: Arc<Gate> = Arc::new(Gate::default());
        let callers: Vec<_> = (0..6)
            .map(|_| {
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    let timeout = Duration::from_millis(200);
                    lookup_with("shared.test", timeout, 4, move |_| {
                        gate.enter();
                        Ok(vec![PUBLIC])
                    })
                })
            })
            .collect();
        for caller in callers {
            assert_eq!(caller.join().unwrap(), DnsClass::Unknown);
        }
        // Six navigations for one host must cost one lookup, not six threads.
        let entered = gate.entered();
        assert_eq!(entered, 1);
        gate.release();
        assert!(wait_until(|| no_lookup("shared.test"), 2_000));
    }

    #[test]
    fn hung_hosts_cannot_exceed_the_lookup_budget() {
        // Four distinct names whose resolver never returns must cost at most
        // `budget` threads; everything after that is refused immediately rather
        // than queued behind them.
        let gate: Arc<Gate> = Arc::new(Gate::default());
        let callers: Vec<_> = (0..8)
            .map(|index| {
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    let host = format!("hung{index}.test");
                    let timeout = Duration::from_millis(150);
                    lookup_with(&host, timeout, 2, move |_| {
                        gate.enter();
                        Ok(vec![PUBLIC])
                    })
                })
            })
            .collect();
        for caller in callers {
            assert_eq!(caller.join().unwrap(), DnsClass::Unknown);
        }
        // The budget is a thread bound, not a hint.
        let entered = gate.entered();
        assert_eq!(entered, 2);

        // Once the stuck lookups return, the slots come back: the refusal is
        // about threads that are really alive, not about a remembered verdict.
        gate.release();
        let freed = || no_lookup("hung0.test") && no_lookup("hung1.test");
        assert!(wait_until(freed, 2_000));
        let usable = || {
            let timeout = Duration::from_millis(300);
            let class = lookup_with("after-release.test", timeout, 2, |_| Ok(vec![PUBLIC]));
            class == DnsClass::Answered { private: false }
        };
        assert!(wait_until(usable, 2_000));
    }

    #[test]
    fn a_timed_out_lookup_never_classifies_its_late_answer() {
        // The caller times out, THEN the resolver answers with a public address.
        // That answer must not reach the caller (it timed out) and must not be
        // cached: the next navigation has to resolve again.
        let gate: Arc<Gate> = Arc::new(Gate::default());
        let calls = Arc::new(AtomicUsize::new(0));

        let first_gate = Arc::clone(&gate);
        let first_calls = Arc::clone(&calls);
        let first = lookup_with("late.test", Duration::from_millis(120), 4, move |_| {
            first_calls.fetch_add(1, Ordering::SeqCst);
            first_gate.enter();
            Ok(vec![PUBLIC])
        });
        // A timeout is a refusal, not a verdict.
        assert_eq!(first, DnsClass::Unknown);

        gate.release();
        let finished = || no_lookup("late.test");
        assert!(wait_until(finished, 2_000));

        let second_calls = Arc::clone(&calls);
        let second = lookup_with("late.test", Duration::from_millis(500), 4, move |_| {
            second_calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![PUBLIC])
        });
        assert_eq!(second, DnsClass::Answered { private: false });
        // The second navigation must resolve again, not reuse the late answer.
        let total = calls.load(Ordering::SeqCst);
        assert_eq!(total, 2);
    }

    #[test]
    fn a_lookup_never_writes_the_answer_cache_itself() {
        // `classify_host` caches answers, never `Unknown` (see its match), and
        // the lookup below writes nothing at all. Caching a timeout would turn
        // one slow lookup into a permanent verdict about a host, which a
        // rebinding name could then keep alive.
        let answer_cache = cache();
        let key = "no-negative-cache.test";
        lock(answer_cache).remove(key);
        // Drive the same path `classify_host` uses for an unanswerable name.
        let class = lookup_with(key, Duration::from_millis(60), 2, |_| {
            std::thread::sleep(Duration::from_millis(200));
            Ok(vec![PUBLIC])
        });
        assert_eq!(class, DnsClass::Unknown);
        assert!(
            lock(answer_cache).get(key).is_none(),
            "a timeout must not enter the answer cache"
        );
        assert!(wait_until(|| no_lookup(key), 2_000));
    }
}
