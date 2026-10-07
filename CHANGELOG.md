# Changelog

Notable user-facing changes are recorded here. Release assets are published from the matching `v*` tag.

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
