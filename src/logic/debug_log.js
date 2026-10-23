// SPDX-License-Identifier: MIT
// Safe projection of the native diagnostics bridge payload. Rust and Kotlin
// already enforce closed vocabularies; this second boundary keeps an invalid
// or future field from entering the visible panel or a user-triggered export.

const MAX_EVENTS = 4_000;
const MAX_DATE_MS = 8_640_000_000_000_000;
const MAX_DURATION_MS = 86_400_000;
const MAX_COUNT = 1_000_000_000;
const MAX_SAFE = Number.MAX_SAFE_INTEGER;

const AREAS = new Set([
  "app", "back", "bridge", "clear", "close", "delete", "devlog", "flush", "ipc",
  "migration", "open", "pip", "profile", "proxy", "queue", "shutdown", "site", "startup",
  "webview",
]);
const PHASES = new Set([
  "back_pressed", "back_step", "capability", "clear", "close", "complete", "cookie_verify",
  "cookie_wipe", "cookie_write", "destroy", "destroy_flush", "drain", "enumerate",
  "flag_write", "flush", "fullscreen", "gate", "generation", "geolocation", "install",
  "geolocation_origins", "geolocation_wipe", "ipc_dispatch", "ipc_setup", "late_callback",
  "launcher_close", "migration", "native_close", "native_fallback", "navigation_start",
  "open", "orphan_sweep", "pip_enter", "pip_playback", "pip_prepare", "pip_restore",
  "pip_transfer", "poll", "profile_bind", "profile_check", "profile_create", "profile_delete",
  "profile_flush", "profile_list", "provider_delete", "quarantine", "quarantine_retire", "ready",
  "queue_remove", "reset", "resolve", "retire", "script", "slot_claim", "start", "storage",
  "sync", "tombstone", "transfer_to_launcher", "view_attach", "view_create", "web_history",
  "verified", "watchdog", "watchdog_arm", "worker_start", "worker_wait",
]);
const RESULTS = new Set([
  "accepted", "absent", "armed", "attached", "available", "blocked", "closed", "copied", "deferred",
  "deleted", "disabled", "duplicate", "empty", "enabled", "entered", "error", "failed",
  "failed_closed", "fallback", "flow_failed", "handled", "intercepted", "late", "lifted",
  "not_entered", "not_needed", "not_restored", "not_empty", "ok", "owed", "paused", "pending",
  "playing", "present", "passed", "quarantined", "queued", "rejected", "refused", "reset", "restored",
  "retired", "settled", "skipped", "started", "stuck", "timeout", "unknown", "unsupported",
  "unsupported_provider", "unavailable", "verified", "watchdog_refused", "supported", "invalid",
]);
const WATCHDOGS = new Set(["armed", "cancelled", "fired", "late", "refused", "unknown"]);
const ERROR_CLASSES = new Set([
  "active_site", "activity_gone", "cookie_unverified", "in_use", "ipc", "invalid_profile",
  "migration_partial", "permission", "provider", "quarantined", "storage", "timeout",
  "unknown", "unavailable", "unsupported", "watchdog_refused",
]);
const OPERATION_ID = /^[rk]-[1-9][0-9]{0,19}$/;

function safeNumber(value, fallback = null, maximum = MAX_SAFE) {
  return Number.isSafeInteger(value) && value >= 0 ? Math.min(value, maximum) : fallback;
}

function safeCode(value, allowed, fallback) {
  return typeof value === "string" && allowed.has(value) ? value : fallback;
}

function safeEvent(raw, index) {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  if (raw.source !== "rust" && raw.source !== "kotlin") return null;

  const sourceSequence = safeNumber(raw.sourceSequence, safeNumber(raw.sequence, index + 1));
  const operationId = typeof raw.operationId === "string" && OPERATION_ID.test(raw.operationId)
    ? raw.operationId
    : null;
  const timestampMs = safeNumber(raw.timestampMs, 0, MAX_DATE_MS);
  const durationMs = safeNumber(raw.durationMs, null, MAX_DURATION_MS);
  const count = safeNumber(raw.count, null, MAX_COUNT);
  const stepId = safeNumber(raw.stepId, null);
  const watchdog = raw.watchdog == null ? null : safeCode(raw.watchdog, WATCHDOGS, "unknown");
  const errorClass = raw.errorClass == null ? null : safeCode(raw.errorClass, ERROR_CLASSES, "unknown");

  return {
    sequence: safeNumber(raw.sequence, index + 1),
    sourceSequence,
    timestampMs,
    source: raw.source,
    area: safeCode(raw.area, AREAS, "site"),
    phase: safeCode(raw.phase, PHASES, "complete"),
    result: safeCode(raw.result, RESULTS, "unknown"),
    ...(operationId ? { operationId } : {}),
    ...(durationMs == null ? {} : { durationMs }),
    ...(watchdog == null ? {} : { watchdog }),
    ...(errorClass == null ? {} : { errorClass }),
    ...(count == null ? {} : { count }),
    ...(stepId == null ? {} : { stepId }),
  };
}

/** Return only the closed, JSON-safe diagnostics schema used by the panel. */
export function sanitizeDebugLogSnapshot(raw) {
  const supplied = raw && typeof raw === "object" && Array.isArray(raw.events)
    ? raw.events
    : [];
  const truncated = Math.max(0, supplied.length - MAX_EVENTS);
  const events = supplied
    .slice(-MAX_EVENTS)
    .map((event, index) => safeEvent(event, index))
    .filter(Boolean);

  events.sort((left, right) =>
    left.timestampMs - right.timestampMs
      || (left.source === right.source ? 0 : left.source === "rust" ? -1 : 1)
      || left.sourceSequence - right.sourceSequence,
  );
  events.forEach((event, index) => {
    event.sequence = index + 1;
  });

  return {
    events,
    droppedEvents: Math.min(
      MAX_SAFE,
      safeNumber(raw?.droppedEvents, 0) + truncated,
    ),
    capacity: Math.max(2_000, safeNumber(raw?.capacity, MAX_EVENTS, MAX_EVENTS)),
  };
}

/** Stable user-triggered export format. Contains codes and counts, not page data. */
export function serializeDebugLogSnapshot(raw) {
  return `${JSON.stringify(sanitizeDebugLogSnapshot(raw), null, 2)}\n`;
}
