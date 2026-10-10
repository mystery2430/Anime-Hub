## AnimeHub v0.3.3

Public production release for Android, Windows and Linux.

### Changes since v0.3.2

- Controlled Android Picture-in-Picture preparation/restoration and video-only play/pause controls.
- Android launcher IPC origin restrictions, fail-closed handling and async dispatch fixes intended to avoid UI-thread waits.
- Android navigation hardening, privacy-conscious diagnostics, and settings save/rollback fixes.
- Accessible modal focus handling and site-card menus.
- Linux key-source tracking to avoid silently switching encryption keys when Secret Service fails.
- Blocking release tests/audit, required Android release signing and signature verification, and checksummed production assets.

See [CHANGELOG](https://github.com/mystery2430/Anime-Hub/blob/v0.3.3/CHANGELOG.md) for details.

### Downloads

Three signed Android release APKs (aarch64, armv7, x86_64), Windows x64 NSIS installer,
Linux x86_64 AppImage, Debian and RPM packages, plus `SHA256SUMS`.
**No CI debug APK is included.** Windows installer is not Authenticode-signed when
Windows signing credentials are absent; SmartScreen may warn.

### Known limitations — please read

- **v0.3.3 için fiziksel Android testi yapılmadı. / No physical Android testing was performed for v0.3.3.** Successful CI compilation is not runtime verification of PiP, IPC, navigation, cookie persistence or performance. No measured performance improvement is claimed.
- **Android’de sağlayıcılar arası aynı-origin localStorage/IndexedDB izolasyonu garanti edilmez. / Provider-to-provider isolation of same-origin localStorage/IndexedDB is not guaranteed on Android.** Best-effort transfer is neither a security boundary nor lossless (secondary indexes and Blob values are not transferred).
- Android navigation protection does not cover blocklist, DNS rebinding, subframes or app-initiated loads.
- Linux Secret Service changes are unit-tested but not manually validated against a real Secret Service.
- Rust dependency audit success is not a guarantee that dependencies have no advisory warnings; see CI. Physical platform testing remains separate from build validation.

v0.3.2 is preserved unchanged.

**Full comparison:** https://github.com/mystery2430/Anime-Hub/compare/v0.3.2...v0.3.3
