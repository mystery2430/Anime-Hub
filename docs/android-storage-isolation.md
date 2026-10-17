# Android: per-site WebView profile isolation

Status: **implemented from source, not device-tested.** This note records the
multi-profile architecture the app ships, the exact migration story for 0.3.4
data, what survives, and what is still unverified. It is not a security
guarantee; CI compiles the aarch64 APK, which is not a device test.

Read with `docs/android-navigation.md` (navigation policy gaps unchanged).

## Architecture

Android keeps exactly one Tauri/Wry WebView — the **launcher** — on the
provider's **default** profile. A site never navigates that WebView. Instead:

```
Rust: open_site → windows::open_site_window (android) ─ under ANDROID_SESSION_LOCK
        │  1. re-validate the registry snapshot
        │  2. close the outgoing site view (profile flushed, view destroyed)
        │  3. run_mobile_open_protocol: profile =
        │     android_profile_name(site_id, generation); one-time migration
        │     is flag-gated and strict (a failed attempt retires that virgin
        │     profile name and retries once)
        ▼
Kotlin: site_migrate → AnimeHubSiteController.migrate    (PHASE 1 — no view)
        │  4. WebViewFeature.MULTI_PROFILE gate → refuse with a sentinel
        │  5. (virgin profile) Profile.getCookieManager(): set cookies →
        │     every callback → flush() on the single flush worker; each
        │     awaited step is watchdog-bounded
        ▼
Rust:   6. `profilemig-<site_id>` is written — durable BEFORE any view
        │     exists. A failed write ABORTS the open: nothing was shown, the
        │     jar is kept, and the next open can safely retry.
        ▼
Kotlin: site_open → AnimeHubSiteController.open          (PHASE 2 — the view)
        │  7. MULTI_PROFILE gate again → refuse with a sentinel
        │  8. WebView() → WebViewCompat.setProfile(view, profile)   ← first API call
        │  9. settings, SiteNavigationGuard, document-start script
        │ 10. content.addView(...) — site view covers the launcher
        │ 11. PiP re-binds to the site view; back callback enabled
        │ 12. loadUrl(url)
        ▼
close  → flush profile cookies (watchdog-bounded; a timeout or a failure
         KEEPS the view and the session ownership for a retry) → removeView →
         destroy → PiP/focus back to launcher (launcher page never moved, so
         revealing it is all there is)
```

**Why two phases.** While the open was a single `site_open` call, the
migration flag was written *after* the WebView existed and a failed write was
only logged. A missing flag then meant "migration pending" even for a profile
the user was already using, so the next open saw the existing profile,
reported `migration-partial`, retired that healthy name and re-imported the
stale jar — losing the site's localStorage/IndexedDB at the next startup
drain. With the flag durable before phase 2, "flag missing" can only mean "no
view has been shown yet", which makes retire-and-retry the safe resolution.
`run_mobile_open_protocol` (in `web/windows.rs`) owns this ordering and is
covered by host-side behaviour tests with a scripted plugin and an injected
storage write failure.

* **Where the Kotlin guarantees live** (one package, no imports between them,
  none of them touching `android.*` or Tauri — which is what lets CI compile and
  *run* them): `SiteProfileOps.kt` holds the shared primitives (the error
  vocabulary mirrored by Rust, the active-session slot, the one-shot step guard
  and the gated geolocation wipe); `ProfileQuarantine.kt` the per-operation
  ledger, the open gate and the retirement rule; `SiteCloseFlow.kt` the
  flush-then-destroy close; `SiteShutdownFlow.kt` the destroy-time flush awaited
  within a budget; `SiteClearFlow.kt` the verified in-place wipe including the
  typed cookie delivery; `SiteMigrationFlow.kt` the one-time jar import;
  `SiteBackStatus.kt` the launcher back-close handshake.
  `AnimeHubSiteController.kt` adapts the real Android/androidx.webkit calls to
  those ports and holds the WebView/PiP/back-button lifecycle.
  `scripts/android_prepare.py` copies every one of them into the generated
  project and **hard-fails** if a file is missing, so the CI unit-test compile
  cannot silently shrink.
* **Launcher**: default profile, `main` origin, IPC-restricted as before.
  Site clear/delete paths never address it (`Default` is rejected by name
  validation; the static `CookieManager.getInstance()` appears only in
  MainActivity's destroy-time flush of the launcher's own jar — which is why it
  is never treated as evidence about a SITE profile's store: the site profile's
  flush is submitted to the worker and awaited inside
  `AnimeHubSiteController.destroy` instead).
* **Site views**: one native `WebView` at a time, layered above the launcher
  via `android.R.id.content.addView`. Adding the view last puts it on top;
  removing it reveals the launcher — the launcher WebView is never removed,
  navigated or profile-assigned.

### Profile names

`android_profile_name(site_id, generation)` in `web/windows.rs`:

```
site-<first 32 hex chars of SHA-256(site_id)>-g<generation>
```

* Derived from the **registered site id only** — never the URL or the display
  name. Renaming a site or editing its URL cannot point two tiles at one
  profile, and a hostile site name cannot influence the profile namespace.
* 128 bits of digest make collisions between different ids negligible; that is
  what "each site id gets a different profile" rests on.
* Charset is `[a-z0-9-]`, conservative for any WebView provider.
* Kotlin (`AnimeHubSiteController.PROFILE_NAME`) re-validates this exact
  shape before every delete/clear/enumerate, so a stray argument can never
  address the `Default` profile or arbitrary provider data.
* The **generation** is a persisted per-site counter (`profilegen-<site_id>`).
  It is bumped *after* a delete was accepted or queued, so a later open of the
  same site id derives a fresh name and cannot collide with an on-disk
  deletion that is still finishing (WebView profile deletion may complete
  asynchronously). The marker is deliberately *not* deleted with the session,
  so even a registry restored from an old backup cannot reuse a queued name.
* Counter hardening (round 3): the bump uses `checked_add` — at `u32::MAX`
  the operation refuses with an error instead of saturating (which would pin
  the counter and reuse the exact queued name forever). A **corrupt** stored
  value is never silently read as `0`: it is recovered to
  `max(generation queued for this site) + 1` (0 only when nothing is queued)
  and persisted immediately. On open, the stored name is additionally checked
  against the queue and advanced past it (`resolve_queued_free_generation`)
  before the profile name is derived.

### `WebViewFeature.MULTI_PROFILE` gate

`AnimeHubSiteController.open` checks `WebViewFeature.isFeatureSupported(
WebViewFeature.MULTI_PROFILE)` before doing anything else. Unsupported → the
open **fails** with the sentinel `unsupported-multi-profile`, which Rust maps
to `AppError::Unsupported` and the launcher shows verbatim:

> Bu cihazın WebView sağlayıcısı çoklu profil (MULTI_PROFILE) desteği
> sunmuyor; site verileri izole edilemediği için site açılamadı. Android
> System WebView veya Google Chrome'u güncelleyin; güncelleme sonrası da
> devam ederse bu cihaz bu özelliği desteklemiyor olabilir.

There is **no fallback** to the shared/default profile: opening "but not
isolated" would silently reintroduce the cross-site mixing this feature
removes.

### Dependency pin (two halves that must move together)

`scripts/android_prepare.py` pins **both** sides of the Kotlin/AndroidX
contract in the generated project, idempotently:

1. `androidx.webkit:webkit:1.16.0` in `app/build.gradle.kts` — the researched
   current stable (May 2026), multi-profile APIs stable since androidx.webkit
   1.9.0, minSdk 24 against the app's pinned `minSdkVersion: 26`; no RC/alpha.
   The template's own version (1.14.0 on Tauri CLI 2.11) is *replaced*, never
   stacked.
2. `kotlin-gradle-plugin:2.1.20` in the root `build.gradle.kts`, raised from
   the template's `1.9.25`.

**Why both:** 1.16.0's POM declares `org.jetbrains.kotlin:kotlin-stdlib:2.1.20`,
and Gradle's highest-wins resolution puts that stdlib on the project
classpath. The template's Kotlin 1.9 compiler cannot read Kotlin 2.1 metadata;
CI reproduced exactly that ("Module was compiled with an incompatible version
of Kotlin — metadata is 2.1.0, expected version is 1.9.0" against
`kotlin-stdlib-2.1.20.jar`). The fix aligns the **compiler** with the stdlib
the project actually resolves — one Kotlin family (2.1.20) for the APK build
*and* for the CI unit-test run, which compiles this same generated project.
Gradle 8.14.3 (template wrapper) and AGP 8.11.0 both support Kotlin 2.1.
This is version alignment, not error suppression: nothing is excluded from
either compile because of the metadata rule. The CI unit-test source-set
isolation removes only the two Tauri-coupled files (`AnimeHubPlugin.kt` and
the generated `MainActivity.kt`); the production chain —
`AnimeHubSiteController.kt`, `AnimeHubPipController.kt`, the split
`AnimeHubDebugLog.kt`/`SiteNavigationGuard.kt`, the pure Kotlin flow files
  (`SiteProfileOps.kt`, `ProfileQuarantine.kt`, `SiteCloseFlow.kt`,
  `SiteShutdownFlow.kt`, `SiteClearFlow.kt`, `SiteMigrationFlow.kt`,
  `SiteBackStatus.kt`),
`SiteNavigationPolicy.kt`, `IpPolicy.kt` — **stays in the test compile**, so
every CI run type-checks the real controller instead of trusting the
APK-step logs.

*Runtime* support depends on the device's WebView provider version, which is
what the feature gate checks.

## What was removed

The JSON transfer machinery is gone from both sides — it was never a security
boundary and is no longer needed:

* Rust: `export_site_storage`, `import_site_storage`, the storage ticks of
  `spawn_session_sync` (`STORAGE_SYNC_EVERY_N_TICKS`), the Android branches of
  `export_cookies`/`import_cookies`, `rollback_android_cookie_swap`,
  `provider_cookie_url`, `parse_android_cookie_header`, and every
  `webview_cookies_*` / `localstorage_*` / `indexeddb_*` bridge call.
* Kotlin: `localstorage_export/import`, `indexeddb_export/import` (and the
  `EXPORT_IDB_JS` dump), `webview_cookies_get/replace`, `seal/openSealed`,
  `flushCookieStoreAsync`.
* UI: the About note claiming same-origin isolation is unsolved (it no longer
  applies on Android; iOS keeps the single shared WebView and stays
  unisolated).

The periodic sync loop now only flushes the **active site profile's** cookie
manager every 30 s (process kills skip the close path); the launcher's own
cookies are untouched.

## Migration from 0.3.4

| 0.3.4 data | Migrated? | How / why |
| --- | --- | --- |
| Encrypted cookie jar (`cookies-<site_id>`) | **Yes, once, strictly** | On the first open of a site under the new version, while the profile does not exist yet: Rust reads the jar (expired cookies pruned) and passes the `Set-Cookie` headers to **`site_migrate`** (phase 1 — no WebView is created); Kotlin writes them through the **profile's** `Profile.getCookieManager()` (never the static default-profile manager) and waits for every `setCookie` callback **and** the background `flush()`, with per-cookie and flush timeouts. Only after that full success does Rust write the `profilemig-<site_id>` flag — **durably, before phase 2 (`site_open`) constructs the WebView and navigates**. A failed flag write therefore aborts the open with a visible, retryable error while nothing has been shown: no orphan view, no split Rust/Kotlin ownership, jar and (missing) flag untouched for the next attempt. Any rejection, exception, lost callback, flush failure — or an already-existing profile, whose live cookies are authoritative and must not be overwritten — fails phase 1 with the `migration-partial` sentinel: Rust queues that virgin name for deletion, bumps the generation and retries **once** on a fresh name; a second failure surfaces a visible, retryable error ("site çerezleri profiline taşınamadı …") with the flag and the jar **both kept**, so the next open starts the migration over. Because the flag is durable before any view exists, a `migration-partial` on reopen can only ever retire a profile the user has **not** seen — a shown profile is never queued or rebuilt from the stale jar. A partially imported profile can therefore never pass as complete, and the legacy jar survives until a real success (or until the user's own clear/remove drops it). |
| `localStorage` snapshot (`localstorage-<site_id>`) | **No** | Only imported when origin verification, completion confirmation and data safety can all be guaranteed — they cannot be, without reviving the origin TOCTOU of the old import path (write lands in whichever document is loaded) and without keeping the JS-eval bridge this change deletes. The blobs are **kept on disk** (not deleted early) until the user clears or removes that site; they are simply never re-injected. |
| IndexedDB snapshot (`indexeddb-<site_id>`) | **No, deliberately** | The dump was lossy (indexes, key paths, structured-clone/Blob values do not round-trip) and is never silently re-imported. Kept on disk like the localStorage snapshot, removed only by the user's own clear/delete. |

**Which sessions survive:** any site whose login state is cookie-borne (all
AniList-backed sites in scope, per `docs/audit-cookie-session-lifecycle.md`)
continues after the one-time jar import — subject to the jar being fresh
enough (it was exported at the last close/30 s sync on 0.3.4). Sites that keep
auth or preferences in `localStorage`/IndexedDB need one re-login / rebuild of
local state; nothing is silently rewritten. After a user-initiated clear or
remove, no legacy data remains (`delete_site_session` also drops the
`profilemig-*` flag so a wiped jar can never be re-imported).

## Deletion / clear semantics

| Action | Site view | Profile | Registry |
| --- | --- | --- | --- |
| Built-in site "remove" | — | **kept** (builtin → hide) | hidden |
| User site remove | closed first, no flush | `ProfileStore.deleteProfile` attempted | removed last; a hard failure keeps the tile |
| Clear one site | closed first, no flush | cleared **in place**: profile cookies (`removeAllCookies`, its delivery read as a typed `CookieWipe` — an empty jar answering "deleted nothing" is a success — plus the final flush), `WebStorage.deleteAllData()`, geolocation permissions | untouched |
| Clear all | active site closed | same in-place clear per registered site + orphan sweep over `ProfileStore.getAllProfileNames()`, restricted to `site-*` names not owned by the current registry | untouched |
| App teardown | the site profile's flush is submitted to the background worker and **awaited within `ShutdownFlow.DESTROY_FLUSH_BUDGET_MS` (1 s)** before the generated Activity kills the process; the view is torn down whatever the outcome; a timeout/failure is logged as *durability not guaranteed* | — | — |

* **One outcome vocabulary, backend to UI** (`remove_site` returns
  `{profile: …}`; `clear_site_data` returns only on a verified clear):
  * `deleted` — the provider accepted the deletion (disk work may still be
    finishing; the generation bumps so the name is never immediately
    reused) or the profile was already absent (nothing left behind);
  * `deferred` — Chromium refused because the profile is loaded in this
    process. The name goes to the encrypted
    `android-pending-profile-deletes` queue (deduped) and drains **at the
    next process start, before any site can open**; the UI says so
    explicitly ("bir sonraki açılışta otomatik silinecek") — it is never
    presented as an instant delete. Anything still pending stays queued;
    the clear-all orphan sweep is the second net. "In use" is the documented
    reason for the `IllegalStateException` that maps to `deferred`, but a
    provider could throw it for another one — so the exception's message is
    written to `Log.w` and the in-app debug log instead of being discarded,
    which keeps a misclassification diagnosable rather than silent (which
    reason a given provider really means is a device-only question; see the
    checklist);
  * `unsupported` — the provider has no profile API (feature gap). The
    command **rejects** with the MULTI_PROFILE guidance, keeps the record
    and queues nothing: a permanent feature gap is never disguised as
    deferred-forever success;
  * `hidden` — built-in site: hidden, profile kept;
  * `notApplicable` — platforms without WebView profiles.
  * `error` — invalid name, active view on that profile, an unexpected
    exception, **or any incomplete clear step**: no success is reported and
    no record/session blob is dropped.
* **Clear coverage (what a success really means):** profile cookies
  (`removeAllCookies`, its delivery interpreted by TYPE — see the cookie-step
  bullet below), every stored
  geolocation permission (enumerated via `getOrigins` and awaited —
  `GeolocationPermissions` has no removeAll-with-callback; an enumeration that
  answers `null` is a **failure**, not an empty set, because "could not
  enumerate" must never be reported as "there was nothing to revoke"), then
  `WebStorage.deleteAllData()` (Web Storage + Web SQL), then a final
  `flush()`. **Every** awaited callback is bounded by a watchdog —
  `STEP_TIMEOUT_MS` (10 s) per step and `FLUSH_TIMEOUT_MS` (15 s) for the
  final flush — and every step is attempted, with the first error aggregated
  and returned. A watchdog that fires before its callback records a timeout
  error, so Rust keeps its encrypted session blobs and the wipe stays
  retryable; a callback arriving *after* the watchdog is ignored and can
  never flip the delivered failure back to success — and, per the next
  bullet, it cannot mutate the profile either. **Not covered:** HTTP
  cache and service-worker registrations —
  no profile-scoped API clears them without a delete/recreate of the whole
  profile (deleting the profile *does* remove them; in-place clear keeps
  the profile). Documented rather than approximated with a
  default-profile-affecting call.
* **The cookie step is read as a typed delivery, not as a boolean.**
  Chromium's contract for `removeAllCookies(callback)` is that the callback
  receives whether **any cookies were deleted** — an outcome report, not an
  error flag. An already-empty jar therefore answers `false` for a wipe that
  fully succeeded, and the old shape (`onDone(ok == true)` in the adapter,
  `!ok` = failure in the flow) read that as a broken clear: the wipe was
  reported failed, Rust kept the encrypted session blobs and never reached
  `delete_site_session`, so a site the user had already cleaned could not drop
  its stored jar and its `profilemig-*` flag. The port now carries
  `CookieWipe`:
  * `Answered(deleted = true)` → success, and the emptiness probe is **not**
    consulted (nothing races a deletion that just completed);
  * `Answered(deleted = false)` → INDETERMINATE, decided by
    `SiteClearPort.cookiesCleared()` (the adapter reads the profile's own
    `CookieManager.hasCookies()`, the only store-wide probe the platform
    offers — `getCookie(url)` needs an origin this path deliberately does not
    carry): provably empty → success; cookies still present → failure;
    `null` (unreadable) → failure. Claiming a wipe we cannot see is the
    dangerous direction, a retryable failure is not;
  * `Failed(reason)` → failure, verbatim in the aggregate error: no profile
    access at that step, or the provider threw, so the call never reached the
    store. This is the case the boolean shape confused with an empty jar, and
    it is never turned into a success. Nothing was issued, so nothing is in
    flight and no quarantine record is created.
  The decision lives in one pure function (`SiteClearFlow.interpretCookieWipe`)
  executed by the CI unit tests; the adapter only supplies the typed delivery
  and the probe. The adapter side is pure too (`CookieWipes`), because two of
  its rules are inversion-sensitive and a mistake in either would manufacture a
  **false success**: a `ValueCallback<Boolean>` may deliver `null` (null is not
  a confirmed deletion, so it becomes `Answered(false)` and the probe decides),
  and `hasCookies()` asks the *opposite* question the flow asks ("are there
  cookies?" vs "is the jar empty?"), so the negation and the throw handling
  (`null` = indeterminate, never "empty") live in tested code instead of inline
  in an adapter that no unit test can instantiate without new dependencies.
* **A timeout bounds the SIDE EFFECT too, not only the answer.** A
  once-latch decides who *reports*; it cannot un-run a mutation. Two
  mechanisms close that gap, one per kind of awaited step
  (`SiteProfileOps.kt` and its split flow files, both executed by the CI unit
  tests):
  * *Gated steps — validity token.* The geolocation wipe is the only step
    whose mutation the app performs itself (because `GeolocationPermissions`
    has no removeAll-with-callback, it awaits `getOrigins` and then calls
    `clear` per origin). That sequencing now lives in the pure
    `GeolocationWipe`, which claims a `StepGuard` **before** the first
    `clear`: the flow's watchdog closes the same guard when it fires, so a
    callback that arrives late performs **zero** `clear` calls and reports
    nothing. Each step carries an `operationId` for the in-app log, so a
    stale callback can be tied to the step that owned it. The enumeration
    answer itself is read three ways: `emptySet()` is a success with nothing to
    revoke, a non-empty set is revoked origin by origin (a throwing `clear`
    fails the step), and `null` — the callback crossing a platform-type
    boundary without a set — is a failure, since the permission state is then
    unknown rather than clean. Claim and mutation
    run on the UI thread — where the watchdog is posted as well — and the
    guard is a CAS, so the race is decided atomically either way.
  * *Watchdog lifetime — armed once, cancelled on settle, fail-fast when it
    cannot be armed.* Every awaited step (`scheduleTimeout` on all three ports)
    now returns its **canceller**, and the flow calls it the moment the step
    settles. Before that, a step whose callback answered normally still left its
    delayed runnable queued for the rest of the window (10 s per step, 15 s per
    flush), and that runnable holds the port — and through it the site WebView
    and the Activity — reachable until it fires; every close, clear and
    migration step paid that. Arming happens in ONE production helper
    (`AnimeHubSiteController.scheduleWatchdog`, `postDelayed` +
    `removeCallbacks`, idempotent and thread-safe). If the handler refuses the
    post, the helper invokes the timeout **immediately** instead of ignoring the
    failure: a flow whose watchdog never fires and whose provider never answers
    would leave its caller — and the Tauri bridge call behind it — waiting
    forever. The step then fails in the conservative direction (for the cookie
    step, the unverified deletion is still quarantined).
  * *Ungated steps — per-operation profile quarantine.* `removeAllCookies` is
    a destructive provider call issued **before** the await, with no
    cancellation API: when its watchdog fires, the deletion may still be in
    flight and can complete *after* the wipe was reported failed — deleting
    cookies that belong to a session which reopened the same profile name (a
    fresh login, or the one-time jar import). Ignoring the late callback
    result does not prevent that, so opening the profile immediately is
    **not** provably safe. The flow therefore registers **that operation** in
    `ProfileQuarantine` under its step guard's `operationId`, and both phases
    of the two-phase open consult `SiteOpenGate` before touching the
    provider: Kotlin rejects with the `profile-quarantined` sentinel, Rust
    maps it to `AppError::Quarantined` with user-facing guidance (never the
    raw token) and propagates it without retiring the profile or persisting
    the migration flag.
  * *Why ownership is per operation, not per profile name.* A name-wide
    blocked/not-blocked flag is not enough: clear **A** times out, clear **B**
    then runs with every step answering, and B's success would lift the block
    that A's still-outstanding deletion is the reason for — the profile opens
    and A's late callback wipes the new session's cookies. The ledger keeps,
    per profile name, the outstanding operation ids with their reasons plus
    two stamps of a monotonic clock (not wall time, which can jump): when the
    last outstanding operation settled, and when a wipe was last fully
    verified. A profile is openable **only** when nothing is outstanding *and*
    the last verification postdates the last settle — so the reopen is
    justified by a final state observed *after* the last destructive operation
    finished, never by a counter merely reaching zero. Consequences, each
    covered by a deterministic Kotlin test running the real flow:
      * a late callback closes **its own** record and nothing else —
        `settle(name, operationId)` returns `false` for an unknown or
        already-closed id, so a provider that delivers the same callback twice
        cannot close somebody else's operation;
      * a verified clear records `verified(name)`; it never erases another
        attempt's outstanding operation, and no production path calls a
        blanket `release(name)` or `reset()`;
      * settling advances the clock, which *invalidates* any earlier
        verification: a late answer arriving after a successful clear cannot
        declare the reopen safe by itself — it leaves the outstanding set
        empty but keeps the "verification predates the last settle" refusal
        standing until one more verified wipe runs.
  * *Verified deletion + generation bump, and the order between them.* The
    delete side is ONE host-tested protocol, `run_mobile_delete_protocol`
    (`web/windows.rs`): derive the name from the **current** generation →
    delete → durably queue a deferred deletion → **durably bump the
    generation** → and only then retire the name's ledger via the separate
    `site_profile_retire` bridge call. Retirement last is the safety property:
    the bump is what makes the name underivable, so if it (or its storage
    write) fails, the command aborts with the ledger **intact** — the name is
    still reachable by a retry or a reopen, and it must therefore still be
    blocked while a `removeAllCookies` deletion may be in flight on it.
    Kotlin used to retire inside `deleteProfileData`, i.e. the moment the
    provider merely *accepted* the deletion and before Rust had written
    anything; a failed bump then left the same name both derivable again and
    unblocked. `deleteProfileData` now retires nothing, `retireProfileQuarantine`
    is the single retirement entry point (refusals decided by the pure
    `ProfileRetirement` rule: an underivable name, or a profile a live session
    is using), and `retire(name)` stays the only way a whole ledger is dropped.
    A **deferred** (`Pending`) deletion keeps every record even after the bump,
    because that profile still exists on disk: the startup drain retires it
    only after it has durably forgotten the queued name. The orphan sweep
    retires what it deleted, and logs (rather than aborting "clear all") if a
    retirement call fails — keeping a ledger is the conservative direction.
    The new generation is a different string and starts unblocked, so an old
    operation cannot mutate the new profile's data.
  * *Shutdown (destroy) is a bounded await, not a fire-and-forget.* The
    generated `MainActivity.onDestroy` ends the process when the Activity is
    finishing, so a flush that was merely *scheduled* could be killed
    mid-write. `ShutdownFlow` (pure, CI-tested) now sequences it: hand the
    site profile's flush to the single background worker (the disk I/O never
    runs on the UI thread) → tear the view down, overlapping the flush →
    await it for `DESTROY_FLUSH_BUDGET_MS` (1 s, deliberately far below the
    15 s in-app flush timeout so teardown cannot ANR). The outcome is one of
    `Completed` / `Failed(reason)` / `TimedOut` / `NotNeeded` and is logged as
    such: a timeout is reported as *durability not guaranteed*, never as a
    landed flush. Each call owns its completion state (a local latch), and the
    session slot is cleared first, so a double teardown flushes nothing the
    second time and an older session's flush cannot be counted for a newer one.
  * *What is deliberately NOT quarantined:* the geolocation step (gated, so a
    late callback provably changes nothing); `WebStorage.deleteAllData()`
    (synchronous — it returned or threw, so the outcome is verified); the
    final `flush()` (it only persists the cookie store that already exists,
    so a late flush cannot delete or grant anything a new session created —
    worst case it durably writes that session's own cookies, which its own
    close flush would do anyway; it still fails the wipe).
  * *Recovery, and why the registry is process-scoped:* the outstanding set
    shrinks only when the provider answers *that* operation; the block lifts
    only once a wipe has also been verified after the last settle — a retry
    whose every step answers, an absent profile, or a deletion the provider
    accepted (whose generation bump was stored durably, after which the name
    is retired) — or on a
    process restart: the late callbacks this defends against belong to this
    process, so persisting the ledger would lock a site out across restarts
    for a hazard that no longer exists. A deferred (in-use) deletion keeps the
    block, and so does an in-flight operation from any earlier attempt.
  * The migration flow faces the same late-write hazard and answers it
    differently, by **retiring** the name (queue + generation bump): a virgin
    import profile is disposable, a profile the user clears in place is not.
* The default profile and the launcher's data are never part of any of these
  paths.

## Preserved behaviour and regressions covered

* **Settings**: the site WebView enables the same surface wry's WebView does
  (JS, DOM/web storage, geolocation, `mediaPlaybackRequiresUserGesture=false`,
  popup-capable scripts).
* **Init script**: installed as a document-start script
  (`WebViewCompat.addDocumentStartJavaScript`), with an `onPageStarted`
  evaluation fallback for providers without `DOCUMENT_START_SCRIPT`. Self-
  guarded, so the fallback can never double-apply.
* **Fullscreen video**: the site view implements `onShowCustomView`/
  `onHideCustomView` (wry's own client declines custom views — this is an
  intentional capability of the new view, not a port of launcher behaviour).
  Back exits fullscreen first, then walks site history, then asks the
  launcher to run the normal `close_site` flow through
  `window.__animehubBackToLauncher` — which returns whether a close session
  *accepted* the request, never a bare success claim. Kotlin then polls
  `window.__animehubBackStatus()` (~200 ms, bounded): `closed` means the
  whole transition finished; `failed:<reason>` keeps the view and the session
  ownership and surfaces the reason with a native Toast (the launcher's own
  notice would sit behind the site view); `pending` keeps waiting (a flush
  may be slow — no second close is started); no answer falls back to a
  native flush-first close with an identity guard, and its failure is also
  surfaced, not swallowed. The launcher JS keeps its label until the
  `animehub://site-closed` event (ownership-checked) — there is no
  `finally`-clear that would report success while Rust still holds the
  session. The task is never finished from the site view.
* **Navigation protection**: the site client is wrapped in the same
  `SiteNavigationGuard`/`SiteNavigationPolicy` as the launcher. The blocklist,
  DNS-rebinding, subframe, redirect and app-initiated-load gaps documented in
  `docs/android-navigation.md` are **unchanged** — this PR does not claim
  otherwise.
* **No `window.ipc` for sites**: the site view never receives a
  `JavascriptInterface`; it uses a plain `WebViewClient`, not wry's
  `RustWebViewClient`. Launcher IPC keeps its existing fail-closed setup.
* **PiP**: `AnimeHubPipController.attach` follows the visible page — site view
  while open, launcher on close/failure/destroy. Activity pause/resume are
  forwarded to the site view; PiP keeps rendering (existing block).
* **Locks**: open/close/sync/remove/clear still serialize on
  `ANDROID_SESSION_LOCK`; the ownership check (`session_is_current`) gates the
  flush loop.
* **Flush awaits are bounded, and the worker is observable**: every flow that
  waits for `CookieManager.flush()` — migration, normal close, clear — arms a
  watchdog (`FLUSH_TIMEOUT_MS`, 15 s; `STEP_TIMEOUT_MS`, 10 s for the other
  clear steps), so a callback that never arrives answers the caller with a
  failure instead of holding the session lock forever. A normal close that
  times out keeps the view AND the session ownership (Rust `current_site`
  and the Kotlin slot stay identical, the close is retryable), and a flush
  callback landing after the timeout is ignored: it can neither flip the
  failure to success nor destroy a view whose ownership was never released.
  All flushes run on ONE single-threaded worker (`FLUSH_EXECUTOR`) — a
  watchdog answers the *caller*; it cannot cancel the blocking provider call
  underneath, so a genuinely stuck flush would pile later flushes up behind
  it. `submitFlush` detects that (a new flush queued while the worker has
  been busy ≥ 10 s) and logs it to logcat and the in-app debug log instead
  of hiding it; no extra threads are spawned to paper over it.

**Behavioural coverage (mocks/injected callbacks, no new dependencies):**

* `SiteProfileOpsTest.kt` (87 cases, executed by the CI Gradle unit-test
  step against the real sequencing code in the pure Kotlin flow files): close
  durability (flush failure keeps the view, resolve failure is an error,
  exactly-once completion, **flush watchdog on a lost callback, a late
  successful flush cannot rescue a timed-out close or destroy the view,
  callback-wins-the-race, retry after a timeout**), clear verification
  (unsupported refuses without touching ports, absent profile, full order,
  cookie/geo/storage/flush failures each fail the clear while later steps
  still run, **the shutdown protocol (submit → teardown → bounded await, the
  four outcomes, a double teardown flushing nothing, independent completion
  state per call, a non-positive budget refused)**, geolocation watchdog on a
  lost callback, **an unanswered (`null`)
  geolocation enumeration fails the wipe, clears nothing and still runs the
  later steps**, **final-flush
  watchdog, stale watchdogs cannot change a delivered result**, never a
  success after a failed flush, and **the typed cookie delivery**: an
  `Answered(false)` over a non-empty jar fails with the precise reason, over an
  **empty** jar it is a verified success that runs every step in order and
  leaves the gate open, over an unreadable store it stays a failure, a
  `Failed(…)` (no profile access / provider threw) is reported verbatim and
  never consults the probe, a confirmed deletion never consults it either,
  `interpretCookieWipe`'s whole decision table is pinned as a pure function, and
  the adapter-side `CookieWipes` mapping is pinned too — null-safe `answered`
  (null takes the indeterminate branch), `failed`/`noProfile` staying typed
  failures even when the probe would say "empty", the `hasCookies()` inversion,
  a throwing probe staying `null`, and all three provider store states driven
  through the real flow end to end),
  the strict migration criterion (virgin-only, every cookie + flush,
  rejection/exception/flush/watchdog failures, exactly-once),
  session-slot rollback after a failed open (stale failure cannot clear a
  newer session), and the back-status decision matrix. These catch the
  real error paths A.1–A.5 were about; the controller only *wires* them.
* **Watchdog arming and cancellation** (4 cases): a settled clear cancels every
  watchdog it armed, a timed-out clear leaves exactly the one that fired queued
  (and a late answer cancels nothing more and changes nothing), a watchdog that
  cannot be armed fails the step instead of hanging the caller while still
  quarantining the unverified deletion, and a settled migration cancels its
  per-cookie and flush watchdogs.
* **Shutdown protocol** (7 cases): submit → teardown → bounded await, the four
  outcomes, a double teardown flushing nothing, independent completion state per
  call, and a non-positive budget refused before anything runs.
* **Timeout side-effect races** (26 of those cases) are measured as data, not
  as callback counts: the fake records every `clear` the production
  `GeolocationWipe` triggers and the origins a session currently holds. They
  cover the callback beating the watchdog (permissions really cleared, no
  block), the watchdog winning and the callback landing late (**zero**
  `clear` calls, one result), a stale callback arriving after the site was
  reopened on the same profile name (the new session's granted permission
  survives), a duplicated delivery (no repeated wipe, no repeated result), a
  port that claimed the guard but reported late (the watchdog's conservative
  failure stands), the `removeAllCookies` timeout → per-operation block →
  refused open, a verified retry that must **not** lift that block while the
  earlier deletion is still outstanding, two independent timeouts whose late
  answers each close only their own record, a late answer arriving *after* a
  verified clear (it empties the outstanding set but the verification is then
  stale, so one more verified wipe is what reopens the site), three
  outstanding operations settling out of order with a verified wipe in the
  middle, a callback delivered three times (its record closes once, another
  operation stays outstanding, one result), a retired name whose bumped
  generation is never blocked and never touched by the old operation, the
  absent-profile path verifying without erasing pending work, the late cookie
  answer closing its record without re-reporting, a **late `Answered(false)`**
  after its watchdog (own record closed, delivered timeout failure unchanged,
  no second report, verification still stale), an **empty-jar success that must
  not lift an older outstanding deletion**, an answered-false cookie
  step failing *without* blocking, a final-flush timeout failing without
  blocking, the unsupported-provider path, and `StepGuard` / `GeolocationWipe`
  / `ProfileQuarantine` / `SiteOpenGate` unit behaviour (including the
  per-operation ledger: duplicate/unknown settles refused, `verified` never
  clearing an outstanding operation, `retire` being the only drop).
* `web/windows.rs` Rust tests drive the real `run_mobile_open_protocol`
  (8 cases) and the real `run_mobile_clear_protocol` (4 cases: a successful
  wipe — including Kotlin's "empty jar" success — reaches
  `delete_site_session` exactly once and drops jar + migration flag, the
  generation counter survives it, a failed wipe propagates and skips the
  session drop so the retry keeps its jar, and only this site's
  current-generation profile name is ever addressed): a scripted plugin half plus the real `SecureStore` wrapped so
  `profilemig-*` writes can fail — the flag-write failure aborts before any
  view, a reopen retires only the never-shown profile and recovers, a shown
  profile is never queued, partial migrations keep the jar and retry once,
  a second partial fails visibly without opening, the MULTI_PROFILE refusal
  touches nothing, an empty/expired jar still records the flag before the
  view, and a `profile-quarantined` refusal in EITHER phase propagates
  without retiring the profile, bumping the generation, queueing a deletion
  or writing the flag (it is not the `migration-partial` path).
* `android_bridge.rs` classifies open refusals host-side
  (`classify_site_open_error`): MULTI_PROFILE → its guidance,
  `profile-quarantined` → `AppError::Quarantined` with the recovery message
  and never the raw sentinel — for **every** wording the Kotlin ledger can
  emit (one outstanding operation, several of them, and a verification that
  predates the last settle) — anything else unchanged; the plugin crate pins
  both sentinel literals against the Kotlin source, asserts the two sentinels
  stay disjoint, and pins the ownership ledger (per-operation `quarantine`,
  `settle`, `verified`, `retire`, the stale-verification condition, and the
  absence of any blanket `release`/`reset` in production Kotlin).
* `tests/back_close.test.js` (13 cases): the launcher close session —
  label survives pending/failed closes (no `finally`-clear), failure
  reason surfaced, no concurrent `close_site`, stale `site-closed` events
  ignored, retry settles to `closed`, and **strict event ownership: an
  owned session ignores `null`/`undefined`/empty payloads, and a delayed
  event from a previous session cannot clear the new one**.

**Static/source coverage** (auxiliary only): `tests/android_isolation.test.js`
(naming, gate ordering, setProfile ordering, **the two-phase migrate → flag →
open ordering and the watchdog-bounded flush awaits**, removed
machinery, deletion/queue ordering, back/PiP/IPC surface, dependency pin),
`gaps.test.js` (prepare pipeline incl. the required copies of the split
files, **CI honesty: the Kotlin test sources must reach the test source set
and the reported `cargo build` status must be the real one**), Rust unit
tests in `web/windows.rs` (name determinism/charset/
uniqueness, strict validator, byte-salvage of a corrupt queue + backup +
repair — **complete tokens only: a multi-digit generation is never truncated
to a shorter valid-looking name, and a generation beyond `u32` is refused
instead of cut down** — corrupt-generation recovery, `checked_add` refusal,
queue resolution, round-trip/dedupe/migration flag), `commands.rs`
(`profile_removal_for` mapping + serialized outcome names), `android-plugin`
crate tests (command surface, controller contract, sentinel literals).
State-machine coverage that needs a real device/provider (actual
`ProfileStore` behaviour, real callback latencies) is **not** claimed here.

## Verification status (honest)

* **Behaviour tests run in CI, device still untested.** The CI Kotlin
  unit-test step executes `SiteProfileOpsTest` (and the policy test) against
  the same generated project the APK uses — real flow code, fake ports — so
  the close/clear/migration error paths above are *executed* on every run,
  and the same step type-checks the production `AnimeHubSiteController.kt`
  left in the source set. The late-callback races are measured as DATA, not
  as callback counts: the test fake records every `clear` the production
  `GeolocationWipe` triggers and the origins a session holds, so "the stale
  callback changed the new session's permissions" is a failing assertion
  rather than an argument. `tests/back_close.test.js` runs with `npm test`.
  This is still not a device: provider-specific behaviour (real
  `ProfileStore`/Chromium timing, MULTI_PROFILE absent) is untested here.
* **Compiled, not device-tested.** The CI "Android compile (aarch64)" job
  builds the debug APK (Kotlin + Rust + Gradle), which proves the code
  compiles and links; it proves nothing about runtime behaviour. No physical
  device has run this build.
* Device checklist that remains: MULTI_PROFILE present/absent behaviour on two
  different WebView provider versions; A-write → B-absent → B-write →
  A-preserved at the same origin; cookie non-mixing across sites; launcher
  default-profile data untouched after clear/remove; back-button ladder
  (fullscreen → history → launcher → task preserved); PiP handoff while a
  site is open; one-time migration showing the old session on first open;
  deferred deletion draining on the next start, and **which exception a real
  provider throws when a deletion cannot run** (the `deferred` classification
  reads `IllegalStateException` as "in use"; the reason is logged, so a device
  run should confirm no other cause is being queued as deferred forever); **real `CookieManager.flush()`
  latency and the watchdog paths on a real provider (a deliberately stuck
  flush: caller answered with a failure, view/session kept, retry works, and
  the `submitFlush` stuck-worker warning appears in the in-app log); the
  two-phase open on a real device (migrate → flag → view), including killing
  the process between phase 1 and the flag write and confirming the next
  open retires only the never-shown profile; the Tauri mobile-invoke
  timeout behaviour for a plugin call whose Kotlin side answered late; and
  the timeout side-effect races on a real provider — a slow `getOrigins`
  answering after the 10 s watchdog (confirm in the site's own UI that a
  permission granted by the reopened session was NOT revoked, and that the
  in-app log shows the stale `operationId`), a slow `removeAllCookies`
  (confirm the reopen is refused with the quarantine guidance, that the
  refusal lifts only after every outstanding deletion has settled AND a
  further wipe was verified, and that no cookie of the new session
  disappears), **two overlapping clears where the first deletion answers only
  after the second one succeeded (confirm the site stays closed until then,
  that the in-app log reports the pending count, and that the first late
  deletion does not remove a cookie the second clear's session created)**,
  a killed process while one or more deletions are outstanding (confirm the
  ledger is gone after restart and the site opens), and **clearing an
  already-clean profile** (the provider answers "no cookies deleted" over an
  empty jar: confirm the clear reports success, the site's stored session jar
  and its `profilemig-*` flag are gone, `profilegen-*` survives, and a site
  whose cookie store cannot be read still reports a retryable failure), and
  **the destroy-time flush on a real provider** (log into a site, exit through
  the PiP window's X or any path that finishes the Activity, and confirm the
  session survives the next launch; check the in-app log shows which outcome
  happened — `destroy-time flush completed` vs `flush unfinished in 1000ms -
  durability not guaranteed` — and that a slow provider flush does not ANR the
  teardown).**
* iOS/macOS: out of scope; the iOS path keeps the shared-WebView flow minus
  the removed snapshot machinery and was not touched beyond that.
