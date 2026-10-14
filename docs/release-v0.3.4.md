## AnimeHub v0.3.4

Public production release for Android, Windows and Linux.

### Changes since v0.3.3

- Session & cookie lifecycle (PR #26): `remove_site` now deletes `profiles/site-*` and closes the live WebView before removing the registry entry (retryable on failure); `clear_site_data` no longer exports live cookies before deletion and is fully serialized against the periodic sync via `DESKTOP_SESSION_LOCK`/`ANDROID_SESSION_LOCK` + `STORE_LOCK` held from the start of the write (including seal). Android active-site wipe now closes without export, clears `current_site` and the shared `CookieManager` while holding the session lock, so the page/sync worker cannot recreate data. `open_site` re-validates the `Site` while holding the lock. Orphan sweep for `clear_site_data(None)` now closes any live WebView even when unregistered before sweeping `profiles/site-*` and propagates `read_dir` errors.
- Security & platform (PR #21): IP policy, PiP/theme and Linux key fixes that were the base for this release (already merged via #21).
- Rust: `rustls 0.23.45 → 0.23.46`, `cargo fmt`/`clippy -D warnings` fixes, Android `aarch64` build green. `npm test` 152 passed.

See [CHANGELOG](https://github.com/mystery2430/Anime-Hub/blob/v0.3.4/CHANGELOG.md) for details.

### Downloads

Three signed Android release APKs (aarch64, armv7, x86_64), Windows x64 NSIS installer,
Linux x86_64 AppImage, Debian and RPM packages, plus `SHA256SUMS`.
**No CI debug APK is included.** Windows installer is not Authenticode-signed when
Windows signing credentials are absent; SmartScreen may warn.

### Known limitations — please read

- **v0.3.4 için fiziksel Android testi yapılmadı. / No physical Android testing was performed for v0.3.4.** Successful CI compilation is not runtime verification of PiP, IPC, navigation, cookie persistence or performance. No measured performance improvement is claimed.
- **Android’de sağlayıcılar arası aynı-origin localStorage/IndexedDB izolasyonu garanti edilmez. / Provider-to-provider isolation of same-origin localStorage/IndexedDB is not guaranteed on Android.** Best-effort transfer is neither a security boundary nor lossless.
- Android navigation protection does not cover blocklist, DNS rebinding, subframes or app-initiated loads.
- Linux Secret Service changes are unit-tested but not manually validated against a real Secret Service.
- Rust dependency audit success is not a guarantee that dependencies have no advisory warnings; see CI. Physical platform testing remains separate from build validation.

v0.3.3 is preserved unchanged.

**Full comparison:** https://github.com/mystery2430/Anime-Hub/compare/v0.3.3...v0.3.4
