// SPDX-License-Identifier: MIT
//
// Shared primitives for the per-site profile flows: the error vocabulary shared
// with Rust, the active-session slot, the one-shot step guard and the gated
// geolocation wipe.
//
// LAYOUT (split so each responsibility — and what may change it — is visible):
//   SiteProfileOps.kt      this file: errors, session slot, step guards, geo wipe
//   ProfileQuarantine.kt   the per-operation ledger, the open gate, retirement
//   SiteCloseFlow.kt       close: flush first, keep everything on failure
//   SiteShutdownFlow.kt    destroy-time flush, awaited within a budget
//   SiteClearFlow.kt       clear: verified in-place wipe, typed cookie delivery
//   SiteMigrationFlow.kt   the one-time 0.3.4 cookie migration
//   SiteBackStatus.kt      the launcher back-close handshake
// All of them are in this one package, so no imports are needed between them.
//
// WHY THEY ARE PURE
// -----------------
// These files must NOT import `android.*` or any Tauri class: the CI unit-test
// source set compiles them with the generated project and runs
// `SiteProfileOpsTest` against them, so the sequencing below is *executed* on
// every run instead of being trusted. The Android API calls live in
// `AnimeHubSiteController.kt`, which adapts them to the ports declared here.
//
// THREADING
// ---------
// Each flow runs its steps on ONE thread at a time: the controller delivers
// every port callback back on the Android UI thread before it reaches a flow.
// The atomics below are what make that safe even if a port ever answered from
// another thread (the documented cross-thread interleaving is a test case).
//
// WHY AWAITING IS NOT ENOUGH ON ITS OWN
// -------------------------------------
// A watchdog answers the CALLER; it cannot cancel the provider operation
// underneath. The steps whose mutation is OURS (geolocation `clear`) are gated
// by [StepGuard], so a late callback provably performs nothing. The steps whose
// mutation is the PROVIDER's and cannot be cancelled (a `removeAllCookies`
// already in flight when the watchdog fired) are handled by [ProfileQuarantine]
// instead — the profile is refused for a new session until that wipe is
// verified, because ignoring the late result would still leave the late
// mutation to land on the new session's data.

package dev.animehub.app

import java.lang.ref.WeakReference
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

/**
 * Sentinels shared with Rust (`animehub_android`). Rust matches these
 * substrings verbatim — changing them requires changing both sides together
 * (the plugin crate asserts the literals against this source).
 */
object SiteProfileErrors {
  /** Provider cannot isolate sites — open/clear refuse, no silent fallback. */
  const val UNSUPPORTED: String = "unsupported-multi-profile"

  /**
   * The pending one-time cookie migration could not complete against a
   * virgin profile this attempt: the profile name must be retired (queued
   * for deletion + generation bump) and the migration retried later. The
   * message format is `migration-partial: <detail>` so Rust can surface the
   * underlying cause while still recognising the sentinel.
   */
  const val MIGRATION_RETRY: String = "migration-partial"

  /**
   * A previous in-place wipe of this profile timed out on a destructive
   * provider call that cannot be cancelled, so its completion is UNVERIFIED:
   * the operation may still mutate the profile after the flow gave up. Kotlin
   * refuses to open the profile for a new session until the wipe is verified
   * (see [ProfileQuarantine]). Message format `profile-quarantined: <reason>`;
   * Rust maps the sentinel onto user-facing guidance instead of showing it raw.
   */
  const val QUARANTINED: String = "profile-quarantined"

  /**
   * A step's watchdog could not be armed on the platform scheduler, so the step
   * was failed WITHOUT starting its provider call. Distinct from a timeout on
   * purpose: a timeout means the provider call was issued and never answered
   * (for `removeAllCookies` that leaves an operation outstanding, which
   * [ProfileQuarantine] tracks), while a refused watchdog means nothing was ever
   * issued — so nothing is outstanding and nothing is quarantined.
   */
  const val WATCHDOG_REFUSED: String = "zamanlayıcı kurulamadı"
}

// ---------------------------------------------------------------------------
// Active site session slot (failed-open rollback, concurrent-view safety)
// ---------------------------------------------------------------------------

/**
 * The single active site WebView slot: view token + its profile name, kept
 * together so a partial open can never strand one of the two.
 *
 * Identity matters: rollback and stale callbacks release ONLY the token they
 * were working with (`releaseIf`), so a failure racing a newer open can never
 * clear the newer session — and a failed open always leaves the slot empty,
 * which is what keeps the next open from hitting a phantom `ERR_ACTIVE_SITE`.
 */
class SiteSessionState<T : Any> {

  private class Entry<T : Any>(val token: WeakReference<T>, val profileName: String)

  @Volatile private var entry: Entry<T>? = null

  /** The active token, or `null` when free (or garbage-collected). */
  val activeToken: T?
    get() = entry?.token?.get()

  /** Profile name paired with the active token (never set on its own). */
  val activeProfile: String?
    get() = if (entry?.token?.get() != null) entry?.profileName else null

  /** `true` when a live token occupies the slot. */
  fun hasActive(): Boolean = entry?.token?.get() != null

  /** Occupy the slot. Fails (and leaves the old value) when occupied. */
  fun acquire(token: T, profileName: String): Boolean {
    if (hasActive()) return false
    entry = Entry(WeakReference(token), profileName)
    return true
  }

  /** Clear the slot only when it still holds [token]. */
  fun releaseIf(token: T): Boolean {
    val current = entry?.token?.get() ?: return if (entry == null) false else releaseStale()
    if (current !== token) return false
    entry = null
    return true
  }

  /** Clear unconditionally (Activity teardown). Returns the released profile. */
  fun clear(): String? {
    val name = activeProfile
    entry = null
    return name
  }

  private fun releaseStale(): Boolean {
    entry = null
    return true
  }
}

// ---------------------------------------------------------------------------
// Awaited steps: the one-shot validity token and the gated geolocation wipe
// ---------------------------------------------------------------------------

/**
 * Whether a step's watchdog could be armed on the platform scheduler.
 *
 * WHY A BARE CANCELLER IS NOT ENOUGH: `Handler.postDelayed` reports a refusal
 * through its RETURN VALUE — `false` means the runnable could not be queued
 * (typically a looper that is exiting) and **nothing throws**. A canceller-only
 * contract could not express that, so a caller could not tell "armed, here is
 * how to drop it" from "there is no watchdog at all". Starting a provider call
 * with no watchdog is how a flow ends up waiting forever: nothing bounds the
 * callback, and the caller — plus the Tauri bridge call behind it — hangs.
 */
sealed interface Watchdog {
  /** Armed. [cancel] drops it; idempotent and safe from any thread. */
  data class Armed(val cancel: () -> Unit) : Watchdog

  /**
   * Not armed. The FLOW owns the consequence: fail the step now and do NOT
   * start the provider work the watchdog was meant to bound.
   */
  object Refused : Watchdog
}

/**
 * The arming decision, pure — so CI executes the platform contract instead of
 * trusting somebody's reading of it: a refusal arrives either as `queued ==
 * false` (the return value) or as a thrown exception, and both must end in
 * [Watchdog.Refused].
 */
object WatchdogArming {
  /** Safe refusal code, or `null` when the runnable was queued. */
  fun refusal(queued: Boolean, thrown: Throwable?): String? =
    if (thrown != null || !queued) "watchdog_refused" else null

  /** [cancel] is handed out only when the runnable really was queued. */
  fun decide(queued: Boolean, thrown: Throwable?, cancel: () -> Unit): Watchdog =
    if (refusal(queued, thrown) == null) Watchdog.Armed(cancel) else Watchdog.Refused
}

/**
 * Validity token for ONE awaited step: the cancellation flag a port must
 * consult before it mutates profile data.
 *
 * WHY A RESULT LATCH IS NOT ENOUGH: a flow's `AtomicBoolean` decides who
 * *reports* — it cannot un-run a side effect. The awaited geolocation wipe
 * enumerates origins asynchronously and performs the `clear` calls itself, so
 * a callback arriving after the watchdog fired used to wipe permissions
 * belonging to whatever session the profile backs by then. [claim] turns that
 * into a single winner for the DATA as well as for the result.
 */
interface StepGuard {
  /**
   * Correlation id for the in-app debug log ("operation id"): a device log
   * line about a stale callback can be tied to the step that owned it.
   */
  val operationId: Long

  /**
   * Atomically win this step. Returns `true` for EXACTLY ONE caller — the
   * flow's watchdog or the port's callback — and `false` for every later one.
   *
   * Contract for ports that mutate: claim FIRST, mutate only on `true`, and
   * report the result in the same serialised block. A caller that got `false`
   * must return without touching provider state and without reporting — the
   * winner's outcome already stands.
   */
  fun claim(): Boolean

  /** Diagnostics only; never a check-then-act substitute for [claim]. */
  val open: Boolean
}

/** Creates [StepGuard]s with process-unique, monotonic [StepGuard.operationId]s. */
object StepGuards {
  private val counter = AtomicLong(0)

  fun create(): StepGuard = Guard(counter.incrementAndGet())

  private class Guard(override val operationId: Long) : StepGuard {
    private val claimed = AtomicBoolean(false)

    override fun claim(): Boolean = claimed.compareAndSet(false, true)

    override val open: Boolean
      get() = !claimed.get()
  }
}

/**
 * The two provider calls the geolocation wipe needs. The real implementation
 * is `profile.geolocationPermissions` (`android.webkit.GeolocationPermissions`);
 * tests implement it over a recording fake, so the side effect the production
 * adapter triggers is measurable in CI.
 */
interface GeolocationApi {
  /** `getOrigins { … }` — asynchronous and read-only; may answer late or never. */
  fun getOrigins(onResult: (Set<String>?) -> Unit)

  /** `clear(origin)` — THE mutation this step exists to gate. */
  fun clear(origin: String)
}

/**
 * Production sequencing of the awaited geolocation wipe, extracted from the
 * controller adapter so CI executes the ordering it must guarantee:
 * await `getOrigins` → **[StepGuard.claim]** → only then `clear` each origin →
 * report.
 *
 * `GeolocationPermissions` has no removeAll-with-callback, which is why this
 * step awaits at all — and awaiting is exactly what makes the mutation
 * cancellable by us: unlike a native `removeAllCookies`, the `clear` calls
 * have not happened yet when the watchdog fires, so refusing them is a real
 * cancellation rather than a shrug. A callback that lost the claim performs
 * ZERO `clear` calls and reports nothing, which keeps a timed-out wipe from
 * revoking permissions a newer session on the same profile just granted.
 *
 * Threading: the controller delivers [GeolocationApi.getOrigins]' callback on
 * the UI thread and posts the watchdog to the UI thread as well, so claim and
 * mutate are one serialised block; the CAS keeps the guarantee even if a port
 * ever answered from another thread.
 *
 * The claim winner always reports (the `clear` loop is wrapped), so a won
 * step can never strand the flow; if `getOrigins` itself throws, the step is
 * claimed and reported as a failure immediately. An enumeration that answers
 * `null` is a failure too — see [clearOrigins]: "could not enumerate" is not
 * "there was nothing to revoke".
 */
object GeolocationWipe {
  fun start(guard: StepGuard, api: GeolocationApi, onDone: (Boolean) -> Unit) {
    if (!guard.open) return // Already settled before we even armed: nothing to do.
    val started =
      try {
        api.getOrigins { origins ->
          if (!guard.claim()) return@getOrigins // Watchdog won: no side effect, no result.
          onDone(clearOrigins(api, origins))
        }
        true
      } catch (_: Throwable) {
        false
      }
    if (!started && guard.claim()) onDone(false)
  }

  /**
   * Clear every enumerated origin. The three provider answers mean three
   * different things, and only one of them is a success:
   *
   *  * `emptySet()` — the profile stores no geolocation permission: a success
   *    with nothing to revoke;
   *  * a non-empty set — revoke each one; a throwing [GeolocationApi.clear] is
   *    a failure (some permission may still be granted);
   *  * `null` — the enumeration produced NO answer, so the state is unknown:
   *    a FAILURE. Android's `GeolocationPermissions.getOrigins(callback)`
   *    documents a `Set<String>` callback, but the port types it nullable
   *    because the value crosses a platform-type boundary (`Set<String!>!`) and
   *    a provider can hand back null. Treating that as "there was nothing to
   *    revoke" reported a clean profile whose permissions were never even
   *    enumerated — the same class of false success the cookie step had.
   */
  private fun clearOrigins(api: GeolocationApi, origins: Set<String>?): Boolean =
    if (origins == null) {
      false
    } else {
      try {
        for (origin in origins) api.clear(origin)
        true
      } catch (_: Throwable) {
        false
      }
    }
}
