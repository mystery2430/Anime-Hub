// SPDX-License-Identifier: MIT
//
// Launcher back-close handshake: what the launcher's `__animehubBackStatus()`
// answer means for Kotlin.
//
// Pure decision/sequencing code: this file must NOT import `android.*` or any
// Tauri class, so the CI unit-test source set compiles and runs it against the
// generated Android project (see SiteProfileOps.kt for the full rationale and
// the one-thread-per-flow rule). Split out of SiteProfileOps.kt so each
// responsibility — and what may change it — is visible at a glance.

package dev.animehub.app

// ---------------------------------------------------------------------------
// Launcher back-close handshake (Kotlin side of window.__animehubBack*)
// ---------------------------------------------------------------------------

/** What the launcher's `__animehubBackStatus()` answer means for Kotlin. */
enum class BackCloseDecision {
  /** A close is in flight — poll again later. */
  POLL,

  /** Rust completed the close (view already destroyed by the chain). */
  CLOSED,

  /** The launcher reported a failed close — surface it, keep everything. */
  FAILED,

  /** No usable answer — fall back to the native close path. */
  FALLBACK,

  /** Still pending after the poll budget — leave the in-flight close alone. */
  GIVE_UP,
}

/**
 * Decisions for polling `window.__animehubBackStatus()` after the back hook
 * was accepted. `null`/`idle`/unknown answers fall back to a native close
 * (the launcher JS may be on an older contract); `pending` polls up to
 * [maxPolls] then gives up without touching state (a flush may simply be
 * slow, and a second close must not race the first).
 */
object SiteBackStatus {
  const val PENDING: String = "pending"
  const val CLOSED: String = "closed"
  const val IDLE: String = "idle"
  const val FAILED_PREFIX: String = "failed:"

  fun decide(status: String?, attempt: Int, maxPolls: Int): BackCloseDecision =
    when {
      status == null || status == IDLE -> BackCloseDecision.FALLBACK
      status == CLOSED -> BackCloseDecision.CLOSED
      status == PENDING ->
        if (attempt < maxPolls) BackCloseDecision.POLL else BackCloseDecision.GIVE_UP
      status.startsWith(FAILED_PREFIX) -> BackCloseDecision.FAILED
      else -> BackCloseDecision.FALLBACK
    }

  /** The message inside a `failed:<msg>` status (empty when absent). */
  fun failureMessage(status: String?): String =
    status?.removePrefix(FAILED_PREFIX)?.takeIf { status.startsWith(FAILED_PREFIX) } ?: ""
}
