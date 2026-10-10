# Android: web storage isolation between providers

Status: **known limitation, not solved.** This note records what the code does
today (verified by reading the source), where the boundary is, and what a real
fix would need. It is not a security guarantee and must not be presented as
one.

## What the current code does

Android runs every site in the single launcher WebView (`open_on_mobile` in
`src-tauri/src/web/windows.rs`). The WebView and its `CookieManager` jar are
shared.

| Data | Mechanism | Isolation between providers |
| --- | --- | --- |
| Cookies | `import_cookies` replaces the shared jar with the target site's saved cookies on every switch; the outgoing jar is exported first | Per provider, by swap. Not a browser profile. A rollback path exists (`rollback_android_cookie_swap`); how it behaves after a process kill mid-swap is not verified |
| `localStorage` | Exported as a sealed blob per site (`localstorage-<site>`) on switch-out and periodically; imported after the target page loads | Origin-scoped by the browser. Providers on **different** hosts never see each other's `localStorage`. Providers on the **same** host share it |
| IndexedDB | Same as `localStorage`, best-effort dump and restore (`indexeddb_export` / `indexeddb_import`) | Origin-scoped by the browser; same-host providers share it. Secondary indexes, key paths and Blobs may not round-trip |
| Cache, service workers | Shared WebView cache | Not isolated |

### Guards that are in place (verified)

- Export only runs while the live page host equals the site being saved:
  - the switch path checks `active_url.host_str() == current.host` before
    exporting (`open_on_mobile`);
  - the periodic sync checks `loaded` (the host) before exporting
    (`spawn_session_sync`).
- Import waits for the page host to equal the target site's host, then
  re-checks `session_is_current` (`import_site_storage`).

### Remaining gaps (verified by reading, not reproduced on a device)

1. **Same-host providers share storage.** Two sites with the same host
   overwrite each other's `localStorage`/IndexedDB snapshots; whichever export
   runs last wins.
2. **Time-of-check / time-of-use in import.** The host check happens in the
   watcher, and the import runs later. A redirect in between could in theory
   write one site's snapshot into another origin. Low risk, not closed.
3. **Snapshot timing.** `localStorage` is saved every N sync ticks, not on every
   write. A crash can lose recent state (documented in `README.md`).
4. **Cache and service workers** are not part of the snapshot at all.

## Why it cannot be fixed with a small patch

`localStorage`, IndexedDB and the HTTP cache are keyed by origin *inside one
WebView profile*. Swapping cookies or copying blobs in and out cannot separate
two same-origin sessions that run in the same profile at the same time. Any
change that tries to do it with copies creates the same data-loss and
cross-contamination risks the current design avoids, and it cannot be verified
without a device.

## Architecture options (for a separate piece of work)

1. **One WebView profile per provider.** The cleanest model: each provider gets
   its own storage. Needs a check of what the pinned WebView version and the
   AndroidX WebKit library offer for multiple profiles on Android. This must be
   verified on the minimum supported WebView, not assumed.
2. **One process per provider.** `WebView.setDataDirectorySuffix` allows one
   data directory per process, which gives isolation at the cost of a separate
   process per site. Needs a launcher/child architecture and a way to hand
   control between processes. Large change.
3. **Keep the current model and document it** (today's state), and treat
   same-host providers as unsupported. Cheapest and honest.

Recommendation: decide between (1) and (3) after checking option (1) on a real
device with the minimum supported WebView. Do not ship any change that is
described as "isolation" until it has been tested on a device.

## What must not happen

- `localStorage.clear()` or any global IndexedDB deletion as part of a switch.
  That would destroy the signed-in state of other providers.
- Writing a snapshot into the current origin without re-checking the host
  immediately before the write.
