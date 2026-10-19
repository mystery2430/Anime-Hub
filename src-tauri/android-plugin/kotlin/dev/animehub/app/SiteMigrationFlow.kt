// SPDX-License-Identifier: MIT
//
// One-time 0.3.4 cookie migration into a virgin profile: strict success,
// retryable failure.
//
// Pure decision/sequencing code: this file must NOT import `android.*` or any
// Tauri class, so the CI unit-test source set compiles and runs it against the
// generated Android project (see SiteProfileOps.kt for the full rationale and
// the one-thread-per-flow rule). Split out of SiteProfileOps.kt so each
// responsibility — and what may change it — is visible at a glance.

package dev.animehub.app

import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

// ---------------------------------------------------------------------------
// One-time 0.3.4 cookie migration: strict success, retryable failure
// ---------------------------------------------------------------------------

/** Operations [SiteMigrationFlow] sequences into a virgin profile. */
interface SiteMigrationPort {
  /** Whether the target profile already exists (live/partial — never touch). */
  fun profileExists(): Boolean

  /** `getOrCreateProfile(name)`; throw = failure. */
  fun createProfile()

  /** One `setCookie(url, cookie) { accepted -> … }` write. */
  fun setCookie(cookie: String, onAccepted: (Boolean) -> Unit)

  /** Blocking `flush()`; `null` = durable. */
  fun flushCookies(onDone: (Throwable?) -> Unit)

  /** One-shot watchdog for the awaited steps. */
  fun scheduleTimeout(millis: Long, onTimeout: () -> Unit): Watchdog
}

/** Result of the migration attempt, with the strict success criterion. */
sealed class MigrationOutcome {
  /** Every cookie accepted AND flush durable → Rust may write the flag. */
  data class Complete(val accepted: Int) : MigrationOutcome()

  /** The profile already existed: its state is authoritative — never write. */
  object ProfileAlreadyExists : MigrationOutcome()

  /**
   * Something failed midway (profile created, cookie rejected/thrown, flush
   * failed, callback lost): the profile is dirty and must be retired.
   * [SiteProfileErrors.MIGRATION_RETRY] prepended by the controller.
   */
  data class Failed(val reason: String, val errorClass: String = "provider") : MigrationOutcome()
}

/**
 * One-time import of the legacy encrypted cookie jar into a *virgin* profile.
 *
 * Success criterion (testable, no interpretation): the profile did not
 * exist, every `setCookie` callback returned accepted, and the flush
 * completed — only then may Rust write `profilemig-<id>` and treat the jar
 * as consumed. Anything else is [MigrationOutcome.Failed] (or
 * [MigrationOutcome.ProfileAlreadyExists]) and the caller maps both to the
 * `migration-partial` sentinel so Rust retires the profile name and retries
 * on a fresh generation — a half-imported profile can never be mistaken for
 * a complete one on the next open, and the legacy snapshot survives until a
 * real success.
 */
object SiteMigrationFlow {
  const val COOKIE_TIMEOUT_MS: Long = 10_000
  const val FLUSH_TIMEOUT_MS: Long = 15_000

  fun run(
    cookies: List<String>,
    port: SiteMigrationPort,
    operationId: String = AnimeHubDebugLog.newOperationId(),
    onDone: (MigrationOutcome) -> Unit,
  ) {
    val safeOperationId = AnimeHubDebugLog.safeOperationId(operationId) ?: AnimeHubDebugLog.newOperationId()
    val flowStartedAt = System.nanoTime()
    val once = AtomicBoolean(false)
    AnimeHubDebugLog.record("migration", "start", "started", safeOperationId, count = cookies.size.toLong())
    fun finish(outcome: MigrationOutcome) {
      if (once.compareAndSet(false, true)) {
        val duration = ((System.nanoTime() - flowStartedAt) / 1_000_000).coerceAtLeast(0)
        val (result, errorClass, count) = when (outcome) {
          is MigrationOutcome.Complete -> Triple("ok", null, outcome.accepted.toLong())
          MigrationOutcome.ProfileAlreadyExists -> Triple("duplicate", "migration_partial", cookies.size.toLong())
          is MigrationOutcome.Failed -> Triple("failed", outcome.errorClass, null)
        }
        AnimeHubDebugLog.record(
          "migration", "complete", result, safeOperationId,
          durationMs = duration, errorClass = errorClass, count = count,
        )
        onDone(outcome)
      }
    }

    AnimeHubDebugLog.record("migration", "profile_check", "started", safeOperationId)
    val exists = runCatching { port.profileExists() }.getOrElse {
      AnimeHubDebugLog.record("migration", "profile_check", "failed", safeOperationId, errorClass = "provider")
      finish(MigrationOutcome.Failed("provider"))
      return
    }
    AnimeHubDebugLog.record("migration", "profile_check", if (exists) "present" else "absent", safeOperationId)
    if (exists) {
      finish(MigrationOutcome.ProfileAlreadyExists)
      return
    }

    AnimeHubDebugLog.record("migration", "profile_create", "started", safeOperationId)
    if (runCatching { port.createProfile() }.isFailure) {
      AnimeHubDebugLog.record("migration", "profile_create", "failed", safeOperationId, errorClass = "provider")
      finish(MigrationOutcome.Failed("provider"))
      return
    }
    AnimeHubDebugLog.record("migration", "profile_create", "ok", safeOperationId)

    val accepted = AtomicInteger(0)
    val index = AtomicInteger(0)

    fun setNext() {
      val i = index.get()
      if (i >= cookies.size) {
        val settled = AtomicBoolean(false)
        val stepStartedAt = System.nanoTime()
        val stepId = cookies.size.toLong()
        AnimeHubDebugLog.record("migration", "flush", "started", safeOperationId, stepId = stepId)
        val flushWatchdog = port.scheduleTimeout(FLUSH_TIMEOUT_MS) {
          if (settled.compareAndSet(false, true)) {
            AnimeHubDebugLog.record(
              "migration", "flush", "timeout", safeOperationId,
              durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
              watchdog = "fired", errorClass = "timeout", stepId = stepId,
            )
            finish(MigrationOutcome.Failed("çerez aktarımı kalıcı hale getirilemedi: zaman aşımı", "timeout"))
          }
        }
        if (flushWatchdog is Watchdog.Refused) {
          // No bound on the flush ⇒ do not start it. Migration remains retryable.
          if (settled.compareAndSet(false, true)) {
            AnimeHubDebugLog.record(
              "migration", "flush", "failed", safeOperationId,
              durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
              watchdog = "refused", errorClass = "watchdog_refused", stepId = stepId,
            )
            finish(MigrationOutcome.Failed("çerez aktarımı kalıcı hale getirilemedi: ${SiteProfileErrors.WATCHDOG_REFUSED}", "watchdog_refused"))
          }
          return
        }
        AnimeHubDebugLog.record("migration", "watchdog_arm", "armed", safeOperationId, stepId = stepId)
        val cancelFlushWatchdog = (flushWatchdog as Watchdog.Armed).cancel
        port.flushCookies { error ->
          if (!settled.compareAndSet(false, true)) {
            AnimeHubDebugLog.record(
              "migration", "late_callback", "late", safeOperationId,
              watchdog = "late", errorClass = "timeout", stepId = stepId,
            )
            return@flushCookies
          }
          cancelFlushWatchdog()
          if (error != null) {
            AnimeHubDebugLog.record(
              "migration", "flush", "failed", safeOperationId,
              durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
              errorClass = "provider", stepId = stepId,
            )
            finish(MigrationOutcome.Failed("provider"))
          } else {
            AnimeHubDebugLog.record(
              "migration", "flush", "ok", safeOperationId,
              durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
              stepId = stepId,
            )
            finish(MigrationOutcome.Complete(accepted.get()))
          }
        }
        return
      }
      val perCookie = AtomicBoolean(false)
      val stepStartedAt = System.nanoTime()
      val stepId = i.toLong()
      AnimeHubDebugLog.record(
        "migration", "cookie_write", "started", safeOperationId,
        count = (i + 1).toLong(), stepId = stepId,
      )
      val cookieWatchdog = port.scheduleTimeout(COOKIE_TIMEOUT_MS) {
        if (perCookie.compareAndSet(false, true)) {
          AnimeHubDebugLog.record(
            "migration", "cookie_write", "timeout", safeOperationId,
            durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
            watchdog = "fired", errorClass = "timeout", count = (i + 1).toLong(), stepId = stepId,
          )
          finish(MigrationOutcome.Failed("çerez aktarımı zaman aşımına uğradı", "timeout"))
        }
      }
      if (cookieWatchdog is Watchdog.Refused) {
        // No bound ⇒ never write this cookie; the incomplete profile is retired.
        if (perCookie.compareAndSet(false, true)) {
          AnimeHubDebugLog.record(
            "migration", "cookie_write", "failed", safeOperationId,
            durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
            watchdog = "refused", errorClass = "watchdog_refused", count = (i + 1).toLong(), stepId = stepId,
          )
          finish(MigrationOutcome.Failed("çerez aktarımı ${SiteProfileErrors.WATCHDOG_REFUSED}", "watchdog_refused"))
        }
        return
      }
      AnimeHubDebugLog.record("migration", "watchdog_arm", "armed", safeOperationId, stepId = stepId)
      val cancelCookieWatchdog = (cookieWatchdog as Watchdog.Armed).cancel
      try {
        port.setCookie(cookies[i]) { ok ->
          if (!perCookie.compareAndSet(false, true)) {
            AnimeHubDebugLog.record(
              "migration", "late_callback", "late", safeOperationId,
              watchdog = "late", errorClass = "timeout", count = (i + 1).toLong(), stepId = stepId,
            )
            return@setCookie
          }
          cancelCookieWatchdog()
          if (!ok) {
            // Strict criterion: any rejected cookie retires the virgin profile.
            AnimeHubDebugLog.record(
              "migration", "cookie_write", "rejected", safeOperationId,
              durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
              errorClass = "migration_partial", count = (i + 1).toLong(), stepId = stepId,
            )
            finish(MigrationOutcome.Failed("çerez reddedildi", "migration_partial"))
            return@setCookie
          }
          accepted.incrementAndGet()
          index.incrementAndGet()
          AnimeHubDebugLog.record(
            "migration", "cookie_write", "ok", safeOperationId,
            durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
            count = accepted.get().toLong(), stepId = stepId,
          )
          setNext()
        }
      } catch (_: Exception) {
        if (perCookie.compareAndSet(false, true)) {
          cancelCookieWatchdog()
          AnimeHubDebugLog.record(
            "migration", "cookie_write", "failed", safeOperationId,
            durationMs = ((System.nanoTime() - stepStartedAt) / 1_000_000).coerceAtLeast(0),
            errorClass = "provider", count = (i + 1).toLong(), stepId = stepId,
          )
          finish(MigrationOutcome.Failed("provider"))
        }
      }
    }
    setNext()
  }
}
