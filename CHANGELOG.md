# Changelog

Notable user-facing changes are recorded here. Release assets are published from the matching `v*` tag.

## [Unreleased]

### Android

- Picture-in-Picture now shows the active player instead of the whole site WebView. A new controlled chain finds the playing video (or, when the player lives in a cross-origin frame, uses that frame as the surface), applies a temporary player-only view of the page, and only then calls `enterPictureInPictureMode()` with the measured aspect ratio and the player's bounds as `sourceRectHint`.
- PiP entry is claimed on API 30+ via `onPictureInPictureRequested()` and on API 26-29 via `onUserLeaveHint()`; both go through the same preparation. Leaving PiP restores the page from byte-exact saved styles, classes and attributes — no DOM move, no reload, playback and scroll position untouched.
- The platform auto-enter flag (`setAutoEnterEnabled(true)`) is deliberately never set: Android documents that it suppresses `onPictureInPictureRequested()`, so the system could enter PiP before the WebView was prepared. The "auto PiP" setting now arms the controlled paths and is re-applied to the native controller on startup.
- If no suitable player is found, if the preparation fails, or if the Activity is not resumed, PiP is not entered and the page is left exactly as it was.
- The generated Android project still carries no permanent PiP code: `scripts/android_prepare.py` installs the configuration and lifecycle after `tauri android init`, and the script now also renders `AnimeHubPipController.kt` from a template and injects the lifecycle block into the generated `MainActivity.kt` idempotently.

### Validation note

The WebView-side controller is covered by `node --test tests/pip_controller.test.js`, the prepare script by `tests/gaps.test.js`, and the Kotlin/plugin invariants by unit tests in `src-tauri` — all green in CI (PR #19, run 37792307795: Android aarch64 compile, three-platform tests, audit and LTO jobs all pass; artifact `animehub-android-aarch64-debug`). The APK builds and installs, but the PiP behaviour itself has **not** been device-tested. Details: [`docs/android-pip.md`](./docs/android-pip.md).

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
