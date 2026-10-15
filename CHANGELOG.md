# Changelog

Notable user-facing changes are recorded here. Production assets are built and checked before the matching tag and public release are created.

## [Unreleased]

### Real per-site WebView profiles on Android

- Sites no longer load inside the launcher's single Tauri/Wry WebView. Each registered site opens in its own native `WebView` layered above the launcher, bound — before any other WebView API call, hierarchy attach or first navigation — to its own persistent androidx.webkit profile (`WebViewCompat.setProfile`), giving every site its own localStorage, IndexedDB, cookies and cache **even when two sites share an origin**. The launcher keeps the default profile and its data is never touched by site clear/delete paths.
- Profile names derive deterministically from the registered `site_id` (`site-<sha256-32>-g<generation>`), never from the URL or display name; Kotlin re-validates the exact shape before every delete/clear so the `Default` profile cannot be addressed. A persisted generation counter plus a deferred-deletion queue (`android-pending-profile-deletes`, drained at startup before any site opens) close the delete-then-recreate race even though profile deletion can finish asynchronously on disk.
- `WebViewFeature.MULTI_PROFILE` is checked at runtime before an open; on unsupported providers the site **refuses to open** with a clear message (update the WebView / the device may not support it). There is no silent fallback to the shared profile.
- The legacy JSON storage-transfer machinery is removed end to end: `export_site_storage`/`import_site_storage`, the periodic localStorage/IndexedDB sync, the Android cookie-swap/rollback bridges (`webview_cookies_*`, `localstorage_*`, `indexeddb_*`), the Kotlin export scripts, and the About note claiming same-origin isolation was unsolved.
- Migration from 0.3.4: the encrypted cookie jar is imported **once**, into a virgin profile's own `Profile.getCookieManager()`, after all callbacks and a background `flush()` complete and before the first navigation (flagged `profilemig-<site>`, never re-applied over a live profile). localStorage/IndexedDB snapshots are **not** re-imported (lossy, origin-TOCTOU); the blobs stay on disk until the user clears or removes that site. Cookie-borne sessions (all in-scope sites) survive; sites keeping auth in web storage need one re-login — documented in `docs/android-storage-isolation.md`.
- Deletion semantics: built-in "remove" still only hides (profile kept); a user site's profile is deleted before the registry entry changes (hard failure keeps the tile, pending refusals queue, no false success); "clear site data" clears profiles in place (profile cookies, web storage, geolocation) and "clear all" adds an orphan sweep over provider profile names restricted to the app's `site-*` namespace. The launcher's default profile is never cleared or deleted.
- Preserved/regressed-covered: site WebView settings mirror wry's (JS/DOM storage/autoplay), the init script runs as a document-start script, the same `SiteNavigationGuard` policy wraps the site client (blocklist/DNS gaps unchanged — not claimed fixed), no `window.ipc`/`JavascriptInterface` on site views, fullscreen video via a custom-view handler, back ladder fullscreen → site history → launcher (`window.__animehubBackToLauncher`, native fallback), and `AnimeHubPipController` re-binds between the site view and the launcher on open/close/failure/destroy.
- `scripts/android_prepare.py` copies the new `AnimeHubSiteController.kt` (required) and reproducibly pins **both halves of the AndroidX/Kotlin contract**: `androidx.webkit:webkit:1.16.0` (researched current stable; multi-profile APIs stable since 1.9.0; replaces the template's version instead of stacking it) **and** `kotlin-gradle-plugin:2.1.20` in the generated root `build.gradle.kts`. 1.16.0 resolves `kotlin-stdlib:2.1.20`, which the template's Kotlin 1.9.25 compiler cannot read — CI reproduced the metadata failure — so the compiler is raised to the same Kotlin family rather than the library being downgraded or files excluded from a compile. The CI unit test runs against this same aligned project.
- Tests: new `tests/android_isolation.test.js` (naming, MULTI_PROFILE gate ordering, setProfile ordering, migration order, removed machinery, deletion/queue ordering, back/PiP/IPC surface, dependency pin, docs honesty), updated `tests/gaps.test.js` (prepare pipeline: controller copy, webkit pin, site lifecycle hooks; obsolete isolation warning gone), Rust unit tests for profile naming/generation/queue/migration flag, and expanded `android-plugin` crate tests for the new command surface.
- **Not device-tested:** CI's aarch64 compile builds the APK but proves no runtime behaviour; the device checklist lives in `docs/android-storage-isolation.md`. The same document records what migrates, what survives, and the honest status.

- Independent review fixes (round 3), all with behavioural tests:
  - **Close durability (A.1):** a normal close now reports success only after the profile's `flush()` completed; a resolve/flush failure keeps the view *and* the session ownership (Kotlin and Rust cannot diverge, and back/close is retryable with the failure surfaced). Activity teardown schedules the flush on the background executor instead of blocking the UI thread or silently skipping it.
  - **Verified clear (A.2):** `clear_site_data` success now means profile cookies (callback checked), every stored geolocation permission (enumerated and awaited), `WebStorage.deleteAllData()` **and** a final flush — each step watched by a timeout, every step attempted, first error aggregated and returned; Rust keeps its encrypted session blobs on any failure so a half-finished wipe is retryable. HTTP cache/service workers are documented as out of in-place-clear scope (no profile-scoped API) rather than approximated.
  - **Retryable 0.3.4 migration (A.3):** cookie import now has a strict success criterion (virgin profile, every `setCookie` accepted, flush durable) with per-cookie and flush timeouts; any partial attempt reports `migration-partial`, which Rust answers by retiring that virgin profile name (queue + generation bump) and retrying once on a fresh name — a second failure surfaces a visible retryable error with the flag and jar kept. Existing profiles are never overwritten by the stale snapshot.
  - **Failed-open rollback (A.4):** the active view/profile are claimed as one slot and released on a failure only if that slot still holds the same view; a failed open can no longer leave a stuck `ERR_ACTIVE_SITE` or clear a newer session.
  - **Honest back-close (A.5):** the launcher hook reports whether a close was *accepted*, Kotlin polls a real status (`pending`/`closed`/`failed:<reason>`), failures surface with a native Toast, the JS label is no longer cleared in a `finally`, and stale `animehub://site-closed` events cannot orphan a newer session.
  - **No silent queue/generation loss (A.6/A.7):** a corrupt pending-delete queue is salvaged name-by-name from the raw bytes, backed up, and repaired — never emptied; a corrupt generation counter is recovered from the queue (never silently `0`) and persisted; the bump uses `checked_add` instead of `saturating_add`; open resolves stored-vs-queued collisions before deriving a name.
  - **One delete/clear outcome vocabulary (A.8):** `remove_site` returns `deleted | deferred | unsupported | hidden | notApplicable` explicitly (deferred says so in the UI; a provider feature gap rejects with the MULTI_PROFILE guidance instead of queueing forever), clear succeeds only on a verified wipe, and the Kotlin-side unsupported refusal classifies through the same Rust path.
  - Tests: `SiteProfileOpsTest.kt` (30 behavioural cases executed by the CI Gradle unit-test step, which now also type-checks the real `AnimeHubSiteController.kt` instead of isolating it away), `tests/back_close.test.js` (11 cases), strict/salvage/generation/overflow Rust tests, `profile_removal_for` mapping tests, and `android_prepare.py` now *requires* the split helper files (`AnimeHubDebugLog.kt`, `SiteNavigationGuard.kt`, `SiteProfileOps.kt`).

## [0.3.4] — 2026-10-14

### Session & cookie lifecycle (PR #26)

- Desktop profile isolation: `remove_site` now deletes `profiles/site-*` and closes the live WebView before removing the registry entry, so a failed close/delete keeps the tile and the user can retry. Built-in sites are hidden, not deleted.
- `clear_site_data` no longer exports live cookies before deletion; the whole close+delete is serialized against the periodic sync via `DESKTOP_SESSION_LOCK`/`ANDROID_SESSION_LOCK` and `STORE_LOCK` is held from the start of the write (including seal), so an in-flight sync cannot resurrect a deleted jar.
- Android: `clear_site_data` for the active site now closes without export, clears `current_site` and the shared `CookieManager` while holding `ANDROID_SESSION_LOCK`, preventing the page/sync worker from recreating data. If the user switched to another site during the operation, the new site is not closed.
- `open_site` re-validates the `Site` snapshot while holding the lifecycle lock, so a concurrent `remove_site` cannot be bypassed with a stale copy.
- `clear_site_data(None)` orphan sweep now propagates `read_dir` errors (missing `profiles` is OK) and, for the all-sites path, closes any live WebView even when its ID is not in the registry snapshot before sweeping `profiles/site-*`.
- Android master-key creation is now fully serialized under `STORE_LOCK` (check+create+write with unique tmp and `fsync`).
- Deterministic race test `concurrent_site_session_write_and_delete_does_not_resurrect` now holds `STORE_LOCK` to simulate an in-flight write before delete and asserts the jar does not reappear.
- Rust: `rustls 0.23.45 → 0.23.46`, `cargo fmt --check` and `clippy -D warnings` fixes, Android `aarch64` build green. `npm test` 152 passed.

### Security & platform (PR #21, base for this release)

- IP policy: numeric IPv4 spellings and documentation ranges refused via IANA special-purpose registries (same vectors in Rust and Kotlin, real policy not regex).
- PiP/theme: optimistic UI with rollback, sequential selection does not let stale async overwrite; PiP native→persist order with compensation on failure.
- Linux key: `symlink_metadata` replaced by no-follow open + fd metadata verification, `AlreadyExists` same policy, no silent key generation on suspicious files.

### Known limitations (unchanged)

- No physical Android device test for v0.3.4; PiP, IPC, navigation, cookie persistence remain device-unverified. Provider-to-provider `localStorage`/`IndexedDB` isolation on Android is still not guaranteed.
- Windows installers are not code-signed without secrets.

## [0.3.3] — 2026-10-10

### Release safety and known limitations

- Production workflows support dispatch and reusable calls and stage installers as CI artifacts. The publisher waits for successful main CI and fresh production builds of that exact SHA before creating a tag or public release. Rust tests and dependency audit failures block validation. Android release signing is required and APK signatures are verified. CI debug APKs are never release assets.
- Local validation: `npm test` **152 passed**, `npm run build` passed, `npm audit` **0 vulnerabilities**. Rust and Android validation runs in GitHub Actions (no local Rust toolchain/Android SDK).
- **No physical Android test was performed for v0.3.3.** PiP, launcher IPC, navigation, cookie persistence and performance remain device-unverified.
- **Provider-to-provider isolation of same-origin localStorage/IndexedDB is not guaranteed on Android.** Best-effort storage transfer is not a security boundary and IndexedDB transfer is not lossless.
- Windows installers are not code-signed unless signing secrets are configured. Android navigation does not cover blocklist, DNS rebinding, subframes or app-initiated loads.

### Changes since v0.3.2

- Security (Android navigation): numeric IPv4 spellings (`2130706433`, `0x7f000001`, `0177.0.0.1`, `127.1`) and the documentation ranges `198.51.100.0/24` and `203.0.113.0/24` are now refused, as the Rust policy refuses them. Blocklist and DNS rebinding are still not applied on Android (documented).
- PiP: the play/pause button is offered only when the player is a `<video>`. For a cross-origin iframe candidate the button is not shown, because the page script cannot control it. Device check pending.
- Security: the launcher's `ipc` object is removed on every path, so a site page never gets it, even when the launcher listener cannot be installed. In that case launcher IPC is off on the device, and the developer log says so (fail closed). Device check pending.
- Settings are applied only after the store accepts them. A failed save no longer leaves the live setting changed, and the switches are redrawn from the saved state. The "auto PiP" switch rolls its saved value back when the native controller fails or refuses (PiP unsupported), and says so. Desktop behaviour is unchanged.
- Settings: the developer panel opens and the log refreshes only after the save succeeds. The blocklist switch saves the saved rule text, not an unsaved draft. The switches are redrawn from the saved state, including the developer and blocklist switches. If the platform cannot be identified, the "auto PiP" switch does not save and does not call the native controller; it says so and reverts. Desktop platforms (Windows, Linux, macOS) keep the preference only. Covered by `tests/settings_flow.test.js`. Local `npm test`: 149 pass, 0 fail.
- Android IPC: `enter_pip`, `set_pip_auto_enter` and `debug_log` run on the async runtime (`#[tauri::command(async)]`), not on the IPC dispatch thread, because they wait for a Kotlin reply. Not device-verified.
- Android: when launcher IPC cannot be installed (fail closed), a native notice tells the user that the in-app connection did not start. Not device-verified.
- Android back affordance: verified from source that the app never produces `animehub://close-site` on Android (the overlay is desktop-only). The refusal blocks no working path. A native Android return path is not built. Documented in `docs/android-navigation.md`.
- Docs: `docs/android-crash-diagnostics.md` gives `adb` steps for a crash or ANR, with redaction rules. Not a verified result.

### Android

- Fixed a freeze (ANR) when the launcher's first native call ran on the UI thread: the `ipc` message listener now hands messages to a worker thread, so a plugin call such as the "auto PiP" setting no longer waits on a blocked UI thread. Found by reading the sources (wry, Tauri, androidx); not reproduced on a device yet.
- Picture-in-Picture now shows the active player instead of the whole site WebView. A new controlled chain finds the playing video (or, when the player lives in a cross-origin frame, uses that frame as the surface), applies a temporary player-only view of the page, and only then calls `enterPictureInPictureMode()` with the measured aspect ratio and the player's bounds as `sourceRectHint`.
- PiP entry is claimed on API 30+ via `onPictureInPictureRequested()` and on API 26-29 via `onUserLeaveHint()`; both go through the same preparation. Leaving PiP restores the page from byte-exact saved styles, classes and attributes — no DOM move, no reload, playback and scroll position untouched.
- The platform auto-enter flag (`setAutoEnterEnabled(true)`) is deliberately never set: Android documents that it suppresses `onPictureInPictureRequested()`, so the system could enter PiP before the WebView was prepared. The "auto PiP" setting now arms the controlled paths and is re-applied to the native controller on startup.
- If no suitable player is found, if the preparation fails, or if the Activity is not resumed, PiP is not entered and the page is left exactly as it was.
- The generated Android project still carries no permanent PiP code: `scripts/android_prepare.py` installs the configuration and lifecycle after `tauri android init`, and the script now also renders `AnimeHubPipController.kt` from a template and injects the lifecycle block into the generated `MainActivity.kt` idempotently.
- Site navigation on Android: the single WebView now has a small navigation guard (`SiteNavigationGuard` in `AnimeHubPlugin.kt`). It cancels top-level navigations to plain `http`, other schemes, and local or private hosts, the same rules the desktop policy applies. Other WebView callbacks are forwarded unchanged. Not covered: blocklist, DNS rebinding, subframes, and app-initiated loads. Not verified on a device. See [`docs/android-navigation.md`](./docs/android-navigation.md).
- Picture-in-Picture window: a play/pause button. It shows the pause icon while the player plays and the play icon while it is paused. Only a press on the button changes playback; the page's own pause is never overridden. Not verified on a device.
- Developer panel log is no longer PiP-only. The in-app buffer (last 200 lines, `AnimeHubDebugLog`) now also records the app start, navigation blocks (scheme only), IPC listener install results, cookie persistence failures, and main-frame load error codes. No URLs or hosts are written. Read it under Settings → Geliştirici seçenekleri.
- Android IPC: wry's `ipc` JavaScript interface, which every loaded page could call, is removed. The launcher gets a `WebMessageListener` that only the `tauri.localhost` origins receive, in the main frame only. If the listener cannot be installed, the error is logged and IPC stays off; no site receives an `ipc` object. Built by the CI Android job; not verified on a device, so launcher IPC is unconfirmed.

### Fixed

- Site cards: the options menu is now a sibling of the card button instead of a button nested inside it (invalid HTML that broke keyboard and screen reader use). Layout and hover behaviour are unchanged.
- Browser-mode settings stub now matches the Rust defaults: `fullscreenSites` is `false` and `theme` is `system`. A test keeps the two in sync.
- Modals and the site action sheet share one overlay stack: Tab stays inside the open dialog, focus returns to the element that opened it, the background is inert while a dialog is open, and Escape closes the top dialog (it previously fell through to "back to launcher" for the action sheet). Escape is ignored during IME composition.
- PiP: a page that throws while the PiP view is applied (in preparation or in a re-assert) is restored instead of being left half-styled.
- Android web storage isolation between providers is documented as a known limitation with architecture options in `docs/android-storage-isolation.md`.
- PiP: the 2-second early-pause retry is removed. A pause that the page observed after PiP was prepared is not undone, because a WebView pause and a user pause cannot be told apart. A player that is paused is left paused; playback continuity during PiP entry is not verified on a device.
- Linux key storage: when the Secret Service is locked, missing or failing, the app no longer creates a second key in `animehub.key`. The key source is recorded in `key-source`. A recorded keyring key fails closed; a recorded file key is kept even after the keyring recovers. Data is never opened or overwritten with a key from another source. Covered by unit tests with an in-memory store; CI `cargo test --all` passed on Linux, Windows and macOS. Not tested against a real Secret Service.
- `src-tauri/capabilities/default.json`: the description now separates the desktop child WebViews from the single Android WebView and records wry's WebView-wide `ipc` JavaScript interface. Permissions and scope are unchanged.
- Documentation: `docs/android-navigation.md` and `docs/android-storage-isolation.md` list their limits.

### Validation note

The WebView-side controller is covered by `node --test tests/pip_controller.test.js`, the prepare script by `tests/gaps.test.js`, and the Kotlin/plugin invariants by unit tests in `src-tauri` — all green in CI (PR #19, run 37792307795: Android aarch64 compile, three-platform tests, audit and LTO jobs all pass; artifact `animehub-android-aarch64-debug`). The APK builds and installs, but the PiP behaviour itself has **not** been device-tested. Details: [`docs/android-pip.md`](./docs/android-pip.md).

Historical validation after `6f28b13` (not the v0.3.3 test count): `npm test` passes locally with 117 tests (`# pass 117`, `# fail 0`). The Rust tests (`cargo test`), the Android build and the Kotlin code were not run locally, because the sandbox has no Rust toolchain or Android SDK. Their results come from GitHub Actions for the pushed commit. Nothing in this section has been tested on a device.

## [0.3.2] — 2026-10-07

### Android

- Route cookie reads and provider cookie swaps through Android's native `CookieManager` bridge. A missing cookie header is treated as an empty jar, and provider navigation waits until the old jar is cleared and the target cookies have been applied.
- Serialize Android provider swaps, session snapshots, close/clear operations, and storage restores; avoid clearing all WebView browsing data on the first provider open.
- Guard plugin WebView work against a destroyed Activity or stale WebView reference.
- Queue `CookieManager.flush()` away from the UI-critical site-open path. This is intended to avoid blocking navigation on cookie-store disk I/O; the performance effect has not yet been measured on a physical device.
- Remove the recurring Android storage-isolation toast shown after opening a site. The About screen still states the current limitation.

### Documentation and limitations

- Clarify that Android cookie jars are swapped per provider, but true provider-level isolation for same-origin `localStorage` and IndexedDB remains unresolved. IndexedDB export/import is best-effort and is not a security boundary.
- No desktop behavior or Tauri/Wry dependency version changed. The lockfile also updates Vite's transitive `source-map-js` from 1.2.1 to 1.2.2 to clear a high-severity development-tool advisory.

### Validation note

The preceding CI debug APK was installed on one Android device: sites opened without crashing, but loading was slow. The asynchronous cookie-persistence change in v0.3.2 has not yet been device-tested, so no site-open performance improvement is claimed.
