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
  data class Failed(val reason: String) : MigrationOutcome()
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

  fun run(cookies: List<String>, port: SiteMigrationPort, onDone: (MigrationOutcome) -> Unit) {
    val once = AtomicBoolean(false)
    fun finish(outcome: MigrationOutcome) {
      if (once.compareAndSet(false, true)) onDone(outcome)
    }
    val exists =
      runCatching { port.profileExists() }.getOrElse { e ->
        finish(MigrationOutcome.Failed("mevcut profili okunamadı: ${e.message}"))
        return
      }
    if (exists) {
      finish(MigrationOutcome.ProfileAlreadyExists)
      return
    }
    runCatching { port.createProfile() }.getOrElse { e ->
      finish(MigrationOutcome.Failed("profil oluşturulamadı: ${e.message}"))
      return
    }

    val accepted = AtomicInteger(0)
    val index = AtomicInteger(0)

    fun setNext() {
      val i = index.get()
      if (i >= cookies.size) {
        val settled = AtomicBoolean(false)
        val flushWatchdog = port.scheduleTimeout(FLUSH_TIMEOUT_MS) {
          if (settled.compareAndSet(false, true)) {
            finish(MigrationOutcome.Failed("çerez aktarımı kalıcı hale getirilemedi: zaman aşımı"))
          }
        }
        if (flushWatchdog is Watchdog.Refused) {
          // No bound on the flush ⇒ do not start it. The strict criterion means
          // this attempt is a retryable failure: the flag is not written, the
          // profile is retired by the caller and the import is retried on a
          // fresh generation — never reported as a complete migration.
          if (settled.compareAndSet(false, true)) {
            finish(
              MigrationOutcome.Failed(
                "çerez aktarımı kalıcı hale getirilemedi: ${SiteProfileErrors.WATCHDOG_REFUSED}"
              )
            )
          }
          return
        }
        val cancelFlushWatchdog = (flushWatchdog as Watchdog.Armed).cancel
        port.flushCookies { error ->
          if (!settled.compareAndSet(false, true)) return@flushCookies
          cancelFlushWatchdog()
          if (error != null) {
            finish(MigrationOutcome.Failed("çerez aktarımı kalıcı hale getirilemedi: ${error.message}"))
          } else {
            finish(MigrationOutcome.Complete(accepted.get()))
          }
        }
        return
      }
      val perCookie = AtomicBoolean(false)
      val cookieWatchdog = port.scheduleTimeout(COOKIE_TIMEOUT_MS) {
        if (perCookie.compareAndSet(false, true)) {
          finish(MigrationOutcome.Failed("çerez aktarımı zaman aşımına uğradı"))
        }
      }
      if (cookieWatchdog is Watchdog.Refused) {
        // No bound ⇒ never write the cookie: an unbounded `setCookie` could
        // answer after this attempt was reported failed, into a profile that is
        // about to be retired, and nothing would notice.
        if (perCookie.compareAndSet(false, true)) {
          finish(MigrationOutcome.Failed("çerez aktarımı ${SiteProfileErrors.WATCHDOG_REFUSED}"))
        }
        return
      }
      val cancelCookieWatchdog = (cookieWatchdog as Watchdog.Armed).cancel
      try {
        port.setCookie(cookies[i]) { ok ->
          if (!perCookie.compareAndSet(false, true)) return@setCookie
          cancelCookieWatchdog()
          if (!ok) {
            // Strict criterion: a rejected cookie fails the migration and
            // stops the chain — the doomed profile gets retired, and writing
            // more of the jar into it would only muddy the retry.
            finish(MigrationOutcome.Failed("çerez reddedildi (${i + 1}/${cookies.size})"))
            return@setCookie
          }
          accepted.incrementAndGet()
          index.incrementAndGet()
          setNext()
        }
      } catch (e: Exception) {
        if (perCookie.compareAndSet(false, true)) {
          cancelCookieWatchdog()
          finish(MigrationOutcome.Failed("çerez aktarımı başarısız: ${e.message}"))
        }
      }
    }
    setNext()
  }
}
