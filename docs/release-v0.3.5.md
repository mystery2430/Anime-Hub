## AnimeHub v0.3.5

Public production release for Android, Windows and Linux.

### Changes since v0.3.4

- **Real per-site WebView profiles on Android (first public release of the model):**
  every site opens in its own native `WebView` bound, before any other WebView
  call, to its own persistent androidx.webkit profile (`MULTI_PROFILE`,
  runtime-checked; unsupported providers refuse the open with a clear message —
  no silent shared-profile fallback). Each site gets its own cookies, `localStorage`,
  IndexedDB and cache **even when two sites share an origin**. The one-time 0.3.4
  encrypted-jar import is strict and retryable (`migration-partial` retires the
  never-shown profile and retries once on a fresh generation); failed opens roll
  back without stranding a session; close/clear/delete outcomes are verified end
  to end (watchdog-bounded flushes, per-operation quarantine, one explicit result
  vocabulary for delete/clear).
- **Hardening consolidation (PR #30):** site-session identity checks on close and
  reopen; DNS-rebinding gates for site webviews on desktop and Android, with
  pinned CONNECT proxies and shared deadlines; secure-store atomic writes;
  diagnostics allowlists with ring caps; capped GraphQL reads; all-or-nothing
  AniList settings save with a touch gate that resets after success; launcher-IPC
  origin gate keyed to the launcher view origin. Android navigation now enforces
  the user blocklist **and** a DNS check for both top-level navigations and
  sub-resources.
- Bug-scan fixes: hiding a built-in site flushes its cookie writes; an AniList
  save resets the touch gate after success.
- `npm test` 302 passed on the release tree; CI green on the release SHA
  (three-platform tests, Android aarch64 compile, release-profile LTO,
  dependency/secret audit). The EasyList-compatible filter engine and
  subscriptions are the next release, not this one.

See [CHANGELOG](https://github.com/mystery2430/Anime-Hub/blob/v0.3.5/CHANGELOG.md) for details.

### Downloads

Three signed Android release APKs (aarch64, armv7, x86_64), Windows x64 NSIS
installer, Linux x86_64 AppImage, Debian and RPM packages, plus `SHA256SUMS`.
**No CI debug APK is included.** Windows installer is not Authenticode-signed
when Windows signing credentials are absent; SmartScreen may warn.

### Known limitations — please read

- **v0.3.5 için fiziksel Android testi yapılmadı. / No physical Android testing
  was performed for v0.3.5.** Successful CI compilation is not runtime
  verification of PiP, IPC, navigation, multi-profile isolation, cookie
  persistence or performance.
- Android DNS/blocklist enforcement runs at connection time through the in-app
  loopback CONNECT proxy, which is deliberately unauthenticated — another
  same-device process can drive it (documented residual risk; the caller could
  reach the same public hosts directly).
- Linux Secret Service changes are unit-tested but not manually validated against
  a real Secret Service.
- Rust dependency audit success is not a guarantee that dependencies have no
  advisory warnings; see CI. Physical platform testing remains separate from
  build validation.

v0.3.4 is preserved unchanged.

**Full comparison:** https://github.com/mystery2430/Anime-Hub/compare/v0.3.4...v0.3.5
