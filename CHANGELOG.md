# Changelog

Notable user-facing changes are recorded here. Release assets are published from the matching `v*` tag.

## [Unreleased]

- Settings are applied only after the store accepts them. A failed save no longer leaves the live setting changed, and the switches are redrawn from the saved state. The "auto PiP" switch rolls its saved value back when the native controller fails or refuses (PiP unsupported), and says so. Desktop behaviour is unchanged.
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

Current branch, after `6f28b13`: `npm test` passes locally with 117 tests (`# pass 117`, `# fail 0`). The Rust tests (`cargo test`), the Android build and the Kotlin code were not run locally, because the sandbox has no Rust toolchain or Android SDK. Their results come from GitHub Actions for the pushed commit. Nothing in this section has been tested on a device.

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
