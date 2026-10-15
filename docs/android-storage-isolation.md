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
        │  4. one-time cookie migration (flag-gated), if any
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

### Dependency pin

`scripts/android_prepare.py` pins `androidx.webkit:webkit:1.15.0` in the
generated `app/build.gradle.kts` (replacing whatever the `tauri android init`
template shipped, idempotently — never stacking a second dependency), minSdk
24 against the app's pinned `minSdkVersion: 26`; no RC/alpha.

**Why not 1.16.0** (checked against the published POMs, and reproduced by CI):
1.16.0 declares `org.jetbrains.kotlin:kotlin-stdlib:2.1.20`, which Gradle's
highest-wins resolution forces onto the generated project; the template's
Kotlin 1.9 compiler cannot read Kotlin 2.1 metadata and the build fails with
*"Module was compiled with an incompatible version of Kotlin"*. 1.15.0 (and
1.14.0, the template's own version) declare no `kotlin-stdlib` at all, so
**1.15.0 is the highest stable, conflict-free release** — and every
multi-profile API this app uses has been stable since androidx.webkit 1.9.0.
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
| Encrypted cookie jar (`cookies-<site_id>`) | **Yes, once** | On the first open of a site under the new version, while the profile does not exist yet: Rust reads the jar (expired cookies pruned), passes the `Set-Cookie` headers to `site_open`; Kotlin writes them through the **profile's** `Profile.getCookieManager()`, waits for every `setCookie` callback, `flush()`es to disk, and only then constructs the WebView and navigates. The `profilemig-<site_id>` flag is written after a successful open; a failed open is retried next time. An existing profile is never overwritten — its live cookies are authoritative. |
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

* `deleteProfile` outcomes: `deleted=true` → accepted (disk work may still be
  finishing; generation bumps so the name is never immediately reused);
  `deleted=false, pending=false` → profile absent (nothing to do);
  `deleted=false, pending=true` → the profile is loaded in this process
  (Chromium refuses to delete an in-use context) or the provider temporarily
  lacks the API. Pending names go to the encrypted
  `android-pending-profile-deletes` queue (deduped) and are drained **at the
  next process start, before any site can open**. Anything still pending
  stays queued; the clear-all orphan sweep is the second net.
* A hard error (unexpected exception, invalid name, active view on that
  profile) rejects: no success is reported and no record is dropped.
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
  Back exits fullscreen first, then walks site history, then returns to the
  launcher via the `window.__animehubBackToLauncher` hook (the normal
  `close_site` flow), and only falls back to a native close if the launcher
  cannot answer. The task is never finished from the site view.
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

Static coverage: `tests/android_isolation.test.js` (naming, gate ordering,
setProfile ordering, migration order, removed machinery, deletion/queue
ordering, back/PiP/IPC surface, dependency pin), `gaps.test.js` (prepare
pipeline incl. the new controller copy and lifecycle hooks), Rust unit tests
in `web/windows.rs` (name determinism/charset/uniqueness, generation
round-trip, queue dedupe, migration flag), `android-plugin` crate tests
(command surface + controller contract).

## Verification status (honest)

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
