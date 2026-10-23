//! Bounded, in-memory, redacted Rust+Kotlin diagnostics for Android emulator
//! testing. The Rust and Kotlin rings are merged by timestamp when read; no
//! diagnostic data is written to disk or sent over the network.

use animehub_android::{DebugEvent, DebugLogSnapshot, DEBUG_LOG_CAPACITY};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const RING_CAPACITY: usize = DEBUG_LOG_CAPACITY as usize;
const COMBINED_CAPACITY: u32 = DEBUG_LOG_CAPACITY * 2;

const AREAS: &[&str] = &[
    "app",
    "back",
    "bridge",
    "clear",
    "close",
    "delete",
    "devlog",
    "flush",
    "ipc",
    "migration",
    "open",
    "pip",
    "profile",
    "proxy",
    "queue",
    "shutdown",
    "site",
    "startup",
    "webview",
];
const PHASES: &[&str] = &[
    "back_pressed",
    "back_step",
    "capability",
    "clear",
    "close",
    "complete",
    "cookie_verify",
    "cookie_wipe",
    "cookie_write",
    "destroy",
    "destroy_flush",
    "drain",
    "enumerate",
    "flag_write",
    "flush",
    "fullscreen",
    "gate",
    "generation",
    "geolocation",
    "geolocation_origins",
    "geolocation_wipe",
    "install",
    "ipc_dispatch",
    "ipc_setup",
    "late_callback",
    "launcher_close",
    "migration",
    "native_close",
    "native_fallback",
    "navigation_start",
    "open",
    "orphan_sweep",
    "pip_enter",
    "pip_playback",
    "pip_prepare",
    "pip_restore",
    "pip_transfer",
    "poll",
    "profile_bind",
    "profile_check",
    "profile_create",
    "profile_delete",
    "profile_flush",
    "profile_list",
    "provider_delete",
    "quarantine",
    "quarantine_retire",
    "queue_remove",
    "ready",
    "reset",
    "resolve",
    "retire",
    "script",
    "slot_claim",
    "start",
    "storage",
    "sync",
    "tombstone",
    "transfer_to_launcher",
    "view_attach",
    "view_create",
    "web_history",
    "verified",
    "watchdog",
    "watchdog_arm",
    "worker_start",
    "worker_wait",
];
const RESULTS: &[&str] = &[
    "accepted",
    "absent",
    "armed",
    "attached",
    "available",
    "blocked",
    "closed",
    "copied",
    "deferred",
    "deleted",
    "disabled",
    "duplicate",
    "empty",
    "enabled",
    "entered",
    "error",
    "failed",
    "failed_closed",
    "fallback",
    "flow_failed",
    "handled",
    "intercepted",
    "late",
    "lifted",
    "not_entered",
    "not_needed",
    "not_restored",
    "not_empty",
    "ok",
    "owed",
    "paused",
    "pending",
    "playing",
    "present",
    "passed",
    "quarantined",
    "queued",
    "rejected",
    "refused",
    "reset",
    "restored",
    "retired",
    "settled",
    "skipped",
    "started",
    "stuck",
    "timeout",
    "unknown",
    "unsupported",
    "unsupported_provider",
    "unavailable",
    "verified",
    "watchdog_refused",
    "supported",
    "invalid",
];
const WATCHDOGS: &[&str] = &["armed", "cancelled", "fired", "late", "refused", "unknown"];
const ERROR_CLASSES: &[&str] = &[
    "active_site",
    "activity_gone",
    "cookie_unverified",
    "in_use",
    "ipc",
    "invalid_profile",
    "migration_partial",
    "permission",
    "provider",
    "quarantined",
    "storage",
    "timeout",
    "unknown",
    "unavailable",
    "unsupported",
    "watchdog_refused",
];

static BUFFER: OnceLock<Mutex<EventBuffer>> = OnceLock::new();
static NEXT_OPERATION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    /// Operation correlation only spans synchronous protocol bodies. Async
    /// command paths create their scope after their final await.
    static CURRENT_OPERATION: RefCell<Option<OperationId>> = const { RefCell::new(None) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationId(String);

impl OperationId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A same-thread diagnostic correlation scope. Drop restores any enclosing id.
pub struct OperationScope {
    previous: Option<OperationId>,
}

impl Drop for OperationScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT_OPERATION.with(|slot| {
            slot.replace(previous);
        });
    }
}

#[derive(Default)]
struct EventBuffer {
    enabled: bool,
    events: VecDeque<DebugEvent>,
    dropped_events: u64,
    next_sequence: u64,
    last_timestamp_ms: u64,
}

impl EventBuffer {
    fn enable(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        if !enabled {
            self.events.clear();
            self.dropped_events = 0;
            return;
        }
        self.push(
            "rust", "devlog", "gate", "enabled", None, None, None, None, None, None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        source: &str,
        area: &str,
        phase: &str,
        result: &str,
        operation_id: Option<String>,
        duration_ms: Option<u64>,
        watchdog: Option<String>,
        error_class: Option<String>,
        count: Option<u64>,
        step_id: Option<u64>,
    ) {
        if !self.enabled {
            return;
        }
        if self.events.len() == RING_CAPACITY {
            self.events.pop_front();
            self.dropped_events = self.dropped_events.saturating_add(1);
        }
        let timestamp_ms = now_ms().max(self.last_timestamp_ms);
        self.last_timestamp_ms = timestamp_ms;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.events.push_back(DebugEvent {
            sequence: self.next_sequence,
            source_sequence: None,
            timestamp_ms,
            source: if source == "kotlin" { "kotlin" } else { "rust" }.into(),
            area: safe_code(area, AREAS, "site"),
            phase: safe_code(phase, PHASES, "complete"),
            result: safe_code(result, RESULTS, "unknown"),
            operation_id: operation_id.filter(|id| valid_operation_id(id)),
            duration_ms,
            watchdog: watchdog.map(|value| safe_code(&value, WATCHDOGS, "unknown")),
            error_class: error_class.map(|value| safe_code(&value, ERROR_CLASSES, "unknown")),
            count,
            step_id,
        });
    }

    fn clear(&mut self) {
        self.events.clear();
        self.dropped_events = 0;
    }
}

fn buffer() -> &'static Mutex<EventBuffer> {
    BUFFER.get_or_init(|| Mutex::new(EventBuffer::default()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn safe_code(value: &str, allowed: &[&str], fallback: &str) -> String {
    if allowed.contains(&value) {
        value.to_owned()
    } else {
        fallback.to_owned()
    }
}

fn valid_operation_id(value: &str) -> bool {
    let Some(digits) = value
        .strip_prefix("r-")
        .or_else(|| value.strip_prefix("k-"))
    else {
        return false;
    };
    !digits.is_empty()
        && digits.len() <= 20
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && !digits.starts_with('0')
}

pub fn new_operation_id() -> OperationId {
    let next = NEXT_OPERATION_ID
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    OperationId(format!("r-{next}"))
}

/// Return the current operation id, or allocate one for a standalone bridge call.
pub fn current_or_new_operation_id() -> OperationId {
    CURRENT_OPERATION
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(new_operation_id)
}

/// Begin a new scope, retaining a parent scope for nested operations.
pub fn begin_operation(area: &str) -> OperationScope {
    begin_operation_with_optional_id(area, None)
}

/// Begin a scope using an externally generated opaque ID when it is valid.
/// Kotlin's back callback uses this to carry one ID through its launcher-JS
/// close request and the synchronous Rust/Kotlin close bridge.
pub fn begin_operation_with_id(area: &str, external_id: &str) -> OperationScope {
    begin_operation_with_optional_id(area, Some(external_id))
}

fn begin_operation_with_optional_id(area: &str, external_id: Option<&str>) -> OperationScope {
    let id = external_id
        .filter(|value| valid_operation_id(value))
        .map(|value| OperationId(value.to_owned()))
        .unwrap_or_else(new_operation_id);
    let previous = CURRENT_OPERATION.with(|slot| slot.replace(Some(id.clone())));
    let scope = OperationScope { previous };
    record_for_operation(
        Some(&id),
        area,
        "start",
        "started",
        None,
        None,
        None,
        None,
        None,
    );
    scope
}

/// Begin a scope only if a caller has not already established correlation.
pub fn scope_if_missing(area: &str) -> Option<OperationScope> {
    CURRENT_OPERATION
        .with(|slot| slot.borrow().is_none())
        .then(|| begin_operation(area))
}

/// An operation id suitable for a plugin payload, as a validated opaque string.
pub fn operation_id_for_bridge() -> String {
    current_or_new_operation_id().0
}

pub fn is_enabled() -> bool {
    buffer().lock().map(|state| state.enabled).unwrap_or(false)
}

/// Gate collection with the persisted `developer_options` setting.
pub fn set_enabled(enabled: bool) {
    if let Ok(mut state) = buffer().lock() {
        state.enable(enabled);
    }
}

/// Record an event correlated to the current synchronous operation.
#[allow(clippy::too_many_arguments)]
pub fn record_current(
    area: &str,
    phase: &str,
    result: &str,
    duration_ms: Option<u64>,
    watchdog: Option<&str>,
    error_class: Option<&str>,
    count: Option<u64>,
    step_id: Option<u64>,
) {
    let operation_id = CURRENT_OPERATION.with(|slot| slot.borrow().clone());
    record_for_operation(
        operation_id.as_ref(),
        area,
        phase,
        result,
        duration_ms,
        watchdog,
        error_class,
        count,
        step_id,
    );
}

/// Record an event against an explicit generated id (used at bridge boundaries).
#[allow(clippy::too_many_arguments)]
pub fn record_for_operation(
    operation_id: Option<&OperationId>,
    area: &str,
    phase: &str,
    result: &str,
    duration_ms: Option<u64>,
    watchdog: Option<&str>,
    error_class: Option<&str>,
    count: Option<u64>,
    step_id: Option<u64>,
) {
    if let Ok(mut state) = buffer().lock() {
        state.push(
            "rust",
            area,
            phase,
            result,
            operation_id.map(|id| id.0.clone()),
            duration_ms,
            watchdog.map(str::to_owned),
            error_class.map(str::to_owned),
            count,
            step_id,
        );
    }
}

/// Record an event without an operation scope (e.g. an app-wide capability).
#[allow(clippy::too_many_arguments)]
pub fn record(
    area: &str,
    phase: &str,
    result: &str,
    duration_ms: Option<u64>,
    watchdog: Option<&str>,
    error_class: Option<&str>,
    count: Option<u64>,
    step_id: Option<u64>,
) {
    record_for_operation(
        None,
        area,
        phase,
        result,
        duration_ms,
        watchdog,
        error_class,
        count,
        step_id,
    );
}

/// Clear both language-side rings when developer options are disabled.
pub fn reset_rust_ring() {
    if let Ok(mut state) = buffer().lock() {
        state.clear();
    }
}

/// Explicit user-triggered reset; a safe reset marker remains visible.
pub fn reset() {
    if let Ok(mut state) = buffer().lock() {
        if state.enabled {
            state.clear();
            state.push(
                "rust", "devlog", "reset", "reset", None, None, None, None, None, None,
            );
        }
    }
}

/// Merge the two process-local rings into one ordered timeline.
pub fn snapshot(kotlin: DebugLogSnapshot) -> DebugLogSnapshot {
    let (mut events, rust_dropped, enabled) = match buffer().lock() {
        Ok(state) => (
            state.events.iter().cloned().collect::<Vec<_>>(),
            state.dropped_events,
            state.enabled,
        ),
        Err(_) => (Vec::new(), 0, false),
    };
    if !enabled {
        return empty_snapshot();
    }
    for mut event in kotlin.events {
        if event.source != "kotlin" {
            continue;
        }
        event.area = safe_code(&event.area, AREAS, "site");
        event.phase = safe_code(&event.phase, PHASES, "complete");
        event.result = safe_code(&event.result, RESULTS, "unknown");
        event.operation_id = event.operation_id.filter(|id| valid_operation_id(id));
        event.watchdog = event
            .watchdog
            .map(|value| safe_code(&value, WATCHDOGS, "unknown"));
        event.error_class = event
            .error_class
            .map(|value| safe_code(&value, ERROR_CLASSES, "unknown"));
        event.duration_ms = event.duration_ms.map(|value| value.min(86_400_000));
        event.count = event.count.map(|value| value.min(1_000_000_000));
        event.step_id = event.step_id.map(|value| value.min(u64::MAX - 1));
        event.source_sequence = Some(event.sequence);
        events.push(event);
    }
    for event in &mut events {
        let source_sequence = event.source_sequence.unwrap_or(event.sequence);
        event.source_sequence = Some(source_sequence);
        event.duration_ms = event.duration_ms.map(|value| value.min(86_400_000));
        event.count = event.count.map(|value| value.min(1_000_000_000));
        event.step_id = event.step_id.map(|value| value.min(u64::MAX - 1));
    }
    events.sort_by(|left, right| {
        left.timestamp_ms
            .cmp(&right.timestamp_ms)
            .then_with(|| source_rank(&left.source).cmp(&source_rank(&right.source)))
            .then_with(|| left.source_sequence.cmp(&right.source_sequence))
    });
    for (index, event) in events.iter_mut().enumerate() {
        event.sequence = (index as u64).saturating_add(1);
    }
    DebugLogSnapshot {
        events,
        dropped_events: rust_dropped.saturating_add(kotlin.dropped_events),
        capacity: COMBINED_CAPACITY,
    }
}

fn source_rank(source: &str) -> u8 {
    match source {
        "rust" => 0,
        "kotlin" => 1,
        _ => 2,
    }
}

pub fn empty_snapshot() -> DebugLogSnapshot {
    DebugLogSnapshot {
        events: Vec::new(),
        dropped_events: 0,
        capacity: COMBINED_CAPACITY,
    }
}

/// Safe, non-textual classification for Rust application errors.
pub fn error_class(error: &crate::error::AppError) -> &'static str {
    use crate::error::AppError;
    match error {
        AppError::InvalidUrl(_) | AppError::InsecureScheme | AppError::Blocked(_) => {
            "invalid_profile"
        }
        AppError::SiteNotFound(_) | AppError::DuplicateName | AppError::DuplicateUrl => "unknown",
        AppError::Storage(_) | AppError::Crypto(_) | AppError::Keyring(_) => "storage",
        AppError::AniList(_) | AppError::Network(_) | AppError::Unauthorized => "provider",
        AppError::Other(_) => "unknown",
        AppError::Unsupported(_) => "unsupported",
        AppError::Quarantined(_) => "quarantined",
        AppError::Icon(_) => "unknown",
        // Fail-closed blocklist transfer: a storage-class failure the user can
        // retry by reopening the site.
        AppError::BlocklistUnavailable => "storage",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_tests() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn event(source: &str, sequence: u64, timestamp_ms: u64, operation_id: &str) -> DebugEvent {
        DebugEvent {
            sequence,
            source_sequence: None,
            timestamp_ms,
            source: source.into(),
            area: "profile".into(),
            phase: "profile_check".into(),
            result: "present".into(),
            operation_id: Some(operation_id.into()),
            duration_ms: Some(1),
            watchdog: None,
            error_class: None,
            count: Some(1),
            step_id: None,
        }
    }

    #[test]
    fn merged_snapshot_orders_mixed_sources_and_preserves_correlation() {
        let _guard = lock_tests();
        set_enabled(false);
        set_enabled(true);
        let op = new_operation_id();
        record_for_operation(
            Some(&op),
            "open",
            "start",
            "started",
            None,
            None,
            None,
            None,
            None,
        );
        let rust = snapshot(DebugLogSnapshot {
            events: vec![event("kotlin", 8, now_ms().saturating_sub(1), op.as_str())],
            dropped_events: 2,
            capacity: DEBUG_LOG_CAPACITY,
        });
        assert!(rust.events.len() >= 2);
        assert_eq!(rust.dropped_events, 2);
        assert_eq!(rust.capacity, COMBINED_CAPACITY);
        assert!(rust
            .events
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
        assert!(rust.events.iter().any(
            |item| item.source == "kotlin" && item.operation_id.as_deref() == Some(op.as_str())
        ));
        assert!(
            rust.events
                .iter()
                .any(|item| item.source == "rust"
                    && item.operation_id.as_deref() == Some(op.as_str()))
        );
        set_enabled(false);
    }

    #[test]
    fn external_operation_ids_are_validated_and_correlated() {
        let _guard = lock_tests();
        set_enabled(false);
        set_enabled(true);
        {
            let _scope = begin_operation_with_id("back", "k-42");
            assert_eq!(current_or_new_operation_id().as_str(), "k-42");
            record_current(
                "back",
                "back_pressed",
                "started",
                None,
                None,
                None,
                None,
                None,
            );
        }
        {
            let _scope = begin_operation_with_id("open", "site-private-profile");
            let generated = current_or_new_operation_id();
            assert!(valid_operation_id(generated.as_str()));
            assert_ne!(generated.as_str(), "site-private-profile");
        }
        let merged = snapshot(DebugLogSnapshot {
            events: Vec::new(),
            dropped_events: 0,
            capacity: DEBUG_LOG_CAPACITY,
        });
        assert!(merged.events.iter().any(|event| {
            event.phase == "back_pressed" && event.operation_id.as_deref() == Some("k-42")
        }));
        let json = serde_json::to_string(&merged).unwrap();
        assert!(!json.contains("site-private-profile"));
        set_enabled(false);
    }

    #[test]
    fn disabled_snapshot_drops_both_rings_and_reports_the_combined_capacity() {
        let _guard = lock_tests();
        set_enabled(false);
        let merged = snapshot(DebugLogSnapshot {
            events: vec![event("kotlin", 9, now_ms(), "k-9")],
            dropped_events: 4,
            capacity: DEBUG_LOG_CAPACITY,
        });
        assert!(merged.events.is_empty());
        assert_eq!(merged.dropped_events, 0);
        assert_eq!(merged.capacity, COMBINED_CAPACITY);
    }

    #[test]
    fn bounded_ring_counts_dropped_events() {
        let mut ring = EventBuffer::default();
        ring.enable(true);
        ring.clear();
        for sequence in 0..(RING_CAPACITY as u64 + 7) {
            ring.push(
                "rust",
                "profile",
                "profile_check",
                "present",
                Some(format!("r-{}", sequence + 1)),
                None,
                None,
                None,
                Some(sequence),
                None,
            );
        }
        assert_eq!(ring.events.len(), RING_CAPACITY);
        assert_eq!(ring.dropped_events, 7);
        assert_eq!(ring.events.front().unwrap().count, Some(7));
    }

    #[test]
    fn disabled_gate_collects_no_events_and_disabling_clears_memory() {
        let mut ring = EventBuffer::default();
        ring.push(
            "rust",
            "profile",
            "profile_check",
            "present",
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(ring.events.is_empty());
        ring.enable(true);
        ring.push(
            "rust",
            "profile",
            "profile_check",
            "present",
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!ring.events.is_empty());
        ring.enable(false);
        assert!(ring.events.is_empty());
        assert_eq!(ring.dropped_events, 0);
    }

    #[test]
    fn unknown_or_sensitive_kotlin_fields_are_replaced_before_export() {
        let _guard = lock_tests();
        set_enabled(false);
        set_enabled(true);
        let mut unsafe_event = event(
            "kotlin",
            1,
            now_ms(),
            "site-0123456789abcdef0123456789abcdef-g8",
        );
        unsafe_event.area = "https://private.example/path?token=secret".into();
        unsafe_event.phase = "profile-name: secret".into();
        unsafe_event.result = "Cookie: secret".into();
        unsafe_event.watchdog = Some("Authorization: Bearer secret".into());
        unsafe_event.error_class = Some("provider said private.example".into());
        unsafe_event.count = None;
        let merged = snapshot(DebugLogSnapshot {
            events: vec![unsafe_event],
            dropped_events: 0,
            capacity: DEBUG_LOG_CAPACITY,
        });
        let json = serde_json::to_string(&merged).unwrap();
        assert!(!json.contains("private.example"));
        assert!(!json.contains("secret"));
        assert!(!json.contains("site-012345"));
        assert!(!json.contains("Authorization"));
        assert!(merged.events.iter().any(|event| {
            event.area == "site"
                && event.phase == "complete"
                && event.result == "unknown"
                && event.operation_id.is_none()
                && event.watchdog.as_deref() == Some("unknown")
                && event.error_class.as_deref() == Some("unknown")
        }));
        set_enabled(false);
    }
}
