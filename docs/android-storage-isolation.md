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
        │  3. profile  = android_profile_name(site_id, generation)
        │  4. one-time cookie migration (flag-gated, strict; a failed
        │     attempt retires that virgin profile name and retries)
        ▼
Kotlin: site_open → AnimeHubSiteController
        │  5. WebViewFeature.MULTI_PROFILE gate → refuse with a sentinel
        │  6. (virgin profile) Profile.getCookieManager(): set cookies →
        │     every callback → flush() on a background thread
        │  7. WebView() → WebViewCompat.setProfile(view, profile)   ← first API call
        │  8. settings, SiteNavigationGuard, document-start script
        │  9. content.addView(...) — site view covers the launcher
        │ 10. PiP re-binds to the site view; back callback enabled
        │ 11. loadUrl(url)
        ▼
close  → flush profile cookies → removeView → destroy → PiP/focus back to
         launcher (launcher page never moved, so revealing it is all there is)
```

* **Launcher**: default profile, `main` origin, IPC-restricted as before.
  Site clear/delete paths never address it (`Default` is rejected by name
  validation; the static `CookieManager.getInstance()` appears only in
  MainActivity's destroy-time flush of the launcher's own jar).
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
`AnimeHubDebugLog.kt`/`SiteNavigationGuard.kt`, `SiteProfileOps.kt`,
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
| Encrypted cookie jar (`cookies-<site_id>`) | **Yes, once, strictly** | On the first open of a site under the new version, while the profile does not exist yet: Rust reads the jar (expired cookies pruned), passes the `Set-Cookie` headers to `site_open`; Kotlin writes them through the **profile's** `Profile.getCookieManager()` (never the static default-profile manager), waits for every `setCookie` callback **and** the background `flush()`, with per-cookie and flush timeouts — and only then constructs the WebView and navigates. The `profilemig-<site_id>` flag is written **only after that full success**. Any rejection, exception, lost callback, flush failure — or an already-existing profile, whose live cookies are authoritative and must not be overwritten — fails the attempt with the `migration-partial` sentinel: Rust queues that virgin name for deletion, bumps the generation and retries **once** on a fresh name; a second failure surfaces a visible, retryable error ("site çerezleri profiline taşınamadı …") with the flag and the jar **both kept**, so the next open starts the migration over. A partially imported profile can therefore never pass as complete, and the legacy jar survives until a real success (or until the user's own clear/remove drops it). |
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
| Clear one site | closed first, no flush | cleared **in place**: profile cookies (`removeAllCookies` + flush), `WebStorage.deleteAllData()`, geolocation permissions | untouched |
| Clear all | active site closed | same in-place clear per registered site + orphan sweep over `ProfileStore.getAllProfileNames()`, restricted to `site-*` names not owned by the current registry | untouched |
| App teardown | view destroyed, profile flushed | — | — |

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
    the clear-all orphan sweep is the second net;
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
  (`removeAllCookies` with the callback result checked), every stored
  geolocation permission (enumerated via `getOrigins` and awaited —
  `GeolocationPermissions` has no removeAll-with-callback), then
  `WebStorage.deleteAllData()` (Web Storage + Web SQL), then a final
  `flush()` — each awaited step has a watchdog and every step is attempted,
  with the first error aggregated and returned. Rust keeps its encrypted
  session blobs whenever that error is non-null, so a half-finished wipe is
  retryable. **Not covered:** HTTP cache and service-worker registrations —
  no profile-scoped API clears them without a delete/recreate of the whole
  profile (deleting the profile *does* remove them; in-place clear keeps
  the profile). Documented rather than approximated with a
  default-profile-affecting call.
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

**Behavioural coverage (mocks/injected callbacks, no new dependencies):**

* `SiteProfileOpsTest.kt` (30 cases, executed by the CI Gradle unit-test
  step against the real sequencing code in `SiteProfileOps.kt`): close
  durability (flush failure keeps the view, resolve failure is an error,
  exactly-once completion), clear verification (unsupported refuses without
  touching ports, absent profile, full order, cookie/geo/storage/flush
  failures each fail the clear while later steps still run, geolocation
  watchdog on a lost callback, never a success after a failed flush),
  the strict migration criterion (virgin-only, every cookie + flush,
  rejection/exception/flush/watchdog failures, exactly-once),
  session-slot rollback after a failed open (stale failure cannot clear a
  newer session), and the back-status decision matrix. These catch the
  real error paths A.1–A.5 were about; the controller only *wires* them.
* `tests/back_close.test.js` (11 cases): the launcher close session —
  label survives pending/failed closes (no `finally`-clear), failure
  reason surfaced, no concurrent `close_site`, stale `site-closed` events
  ignored, retry settles to `closed`.

**Static/source coverage** (auxiliary only): `tests/android_isolation.test.js`
(naming, gate ordering, setProfile ordering, migration order, removed
machinery, deletion/queue ordering, back/PiP/IPC surface, dependency pin),
`gaps.test.js` (prepare pipeline incl. the required copies of the split
files), Rust unit tests in `web/windows.rs` (name determinism/charset/
uniqueness, strict validator, byte-salvage of a corrupt queue + backup +
repair, corrupt-generation recovery, `checked_add` refusal, queue
resolution, round-trip/dedupe/migration flag), `commands.rs`
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
  left in the source set. `tests/back_close.test.js` runs with `npm test`.
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
  deferred deletion draining on the next start.
* iOS/macOS: out of scope; the iOS path keeps the shared-WebView flow minus
  the removed snapshot machinery and was not touched beyond that.
