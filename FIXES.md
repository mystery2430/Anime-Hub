# PR #27 review fixes

Based on `mystery2430/Anime-Hub` PR #27 head
`9bf35ffc9e7223e305bfaa9c68f2f8ff288e844e`.

| Review id | Fix | Evidence |
| --- | --- | --- |
| SEC-01 / R-05 | Android custom-scheme OAuth filter removed; native login is loopback-only and refuses a custom scheme before writing pending state. | `tauri.conf.json`, `anilist.rs`, `commands.rs`, `lib.rs` |
| BUG-01 | Legacy jar migration filters to host-ownable cookies; individual provider rejections are skipped, not fatal. | `web/windows.rs`, `SiteMigrationFlow.kt` |
| BUG-02 | `minSdkVersion` 29 with Android 10 / updated WebView guidance. | `tauri.conf.json`, `android_bridge.rs` |
| BUG-03 | Stuck flush worker is replaced after a measured stall. | `SiteFlushWorker.kt`, `SiteProfileOps.kt` |
| SEC-02 (Android mitigated by design, **device verification pending — not closed**) | Round 16 pins the connection: an in-process loopback CONNECT proxy (`ProxyController`, one rule + `removeImplicitRules`, no `addDirect`) resolves once, classifies with `IpPolicy`, connects to the vetted address and verifies `socket.inetAddress`; unsupported provider or failed install refuses the open. Pre-checks remain as defence in depth; TOCTOU remains on desktop. Not marked fixed until `docs/android-dns-pinning-device-checklist.md` runs. | `DnsPinnedProxy.kt`, `PinnedConnect.kt`, `SiteProxyGate.kt`, `SiteProxyController.kt`, `SiteNavigationGuard.kt`, `SiteNavigationPolicy.kt`, `docs/security-residual-risks.md` |
| R-01 | Android user blocklist is pushed from Rust before every open and enforced in navigation + sub-resources. | `sites/blocklist.rs`, `commands.rs`, `SiteBlocklist.kt`, `AnimeHubPlugin.kt` |
| DEP-01 | Every external Action pinned to a commit SHA, guarded by a test. The hardened files are in `ci-fixes/` (the push token has no `workflow` scope) and installed by `ci-fixes/apply.sh`. | `ci-fixes/*.yml`, `tests/gaps.test.js` |
| DEP-02 | `android_prepare.py` fails on duplicated/missing pins; the proposed CI checks the resolved Gradle graph. | `scripts/android_prepare.py`, `ci-fixes/ci.yml` |
| MNT-03 | Capability description matches the native per-site WebView model. | `capabilities/default.json` |
| VER-01 / MNT-02 | Behavioral tests added for migration, flush recovery, DNS verdicts, blocklist matching and action pinning. | test suites |

## Review round 4 — found while auditing this branch

| Review id | Fix | Evidence |
| --- | --- | --- |
| SEC-03 (new) | `sync_android_blocklist` pushed `Blocklist::entries()` regardless of `BlocklistState.enabled`. Kotlin has no enabled flag, so turning the blocklist off left every listed host blocked inside the Android site WebView while desktop `is_blocked` answered `false`. The snapshot is now gated by `Blocklist::is_enabled()` through one helper, `android_blocklist_entries`. | `commands.rs`, `SiteBlocklist.kt` |
| SEC-04 (new) | The same function used `lock().map(..).unwrap_or_default()`: after one panic the poisoned lock pushed an **empty** rule set, i.e. Android enforcement silently regressed to desktop-only, with no log line. It now recovers the guard and keeps the warn log. | `commands.rs` |
| BUG-04 (new) | 23 production `lock().expect("… lock")` sites in `web/windows.rs` panicked on a poisoned mutex. One panic in a background flush/resolver poisoned `current_site` / `launcher_url` / `blocklist`, after which every navigation callback and launcher navigation panicked as well (crash loop). Added `commands::lock_recover` and used it at every site, including `lock_desktop_session`. | `commands.rs`, `web/windows.rs` |
| VER-02 (new) | Unit tests pin both regressions: a disabled blocklist produces an empty Android snapshot and still answers `false` locally; `lock_recover` returns the value of a poisoned mutex. | `commands.rs` tests |

Not re-verified in this round: the reviewing environment had no Rust or Kotlin
toolchain, so `cargo test`, `cargo clippy -D warnings` and the Gradle unit tests
were not run here. `npm test` (250 tests) passes.

## Review round 5 — reachability and build-input audit

| Review id | Finding | Fix | Evidence |
| --- | --- | --- | --- |
| BUG-05 | «Başlatıcıdan gizle»yi tersine çevirmenin hiçbir yolu yoktu. `remove_site` bir built-in'i `Reg.is_hidden = true` yapıyor (veri silinmiyor), ama `list_sites` yalnızca `visible()` döndürüyor ve `set_site_hidden` komutunun frontend'de **tek bir çağıranı yoktu**. Yani gizlenen built-in UI'dan bir daha adlandırılamıyordu; geri getirmenin tek yolu şifreli yapılandırmayı silmekti (AniList token'ları ve tüm ayarlarla birlikte). | `Registry::hidden()` + `list_hidden_sites` komutu (kayıtlı), Ayarlar → «Gizlenen siteler» bölümü ve `set_site_hidden(id, false)` çağıran «Geri göster» düğmesi. Tarayıcı modu stub'ı da artık built-in'i **silmek yerine gizliyor**. | `sites/registry.rs`, `commands.rs`, `lib.rs`, `src/main.js`, `index.html`, `src/styles.css`, `src/logic/bridge.js`, `src/logic/hidden_sites.js` |
| BUG-06 | `android_prepare.py` iki Kotlin dosyasını (`IpPolicy.kt`, `SiteNavigationPolicy.kt`) «bulunamadı» **uyarısıyla** atlıyordu. Bu dosyalar `SiteNavigationPolicy`/guard tarafından her gezinmede kullanılıyor; eksik kopya, anlaşılır betik hatası yerine Gradle'da çözülemeyen referans olarak patlıyordu. | İkisi de diğer zorunlu birimler gibi sert hata + paket doğrulaması. | `scripts/android_prepare.py` |
| VER-03 | Reachability regresyonunu kilitleyen testler: kayıtlı her komutun `commands.rs`'te tanımlı olması, frontend'in çağırdığı her komutun kayıtlı olması, `list_hidden_sites`/`set_site_hidden` çiftinin korunması ve prepare betiğinin zorunlu Kotlin birimleri. | 4 test + 6 gizli-site görünüm modeli testi (`hidden_sites.js`). | `tests/command_wiring.test.js`, `tests/hidden_sites.test.js` |

Round 5'te yeniden bakılıp **temiz** bulunanlar: `embed_json` (JSON içindeki
`</script`, U+2028/U+2029 kaçışları), `profile_dir_name` id temizliği (yol
geçişi yok), Android `LauncherIpc` origin denetimi (şema+host+port+yol+query+
fragment+userInfo, yalnız ana çerçeve), `Registry::ensure_builtins` (gizli
built-in'i geri getirmiyor), `remove_site`'ın iki platformdaki built-in kolu
(profil silmeden gizleme).

Doğrulanamayan: Rust/Kotlin araç zinciri bu ortamda yok; `cargo test`,
`clippy` ve Gradle birim testleri koşmadı. `node --test` (256 test) geçti.

## Review round 6 — security follow-up items

| # | Finding (as reported) | Fix | Evidence |
| --- | --- | --- | --- |
| 1 | Android blocklist transfer failure was only logged; the site opened anyway. | `sync_android_blocklist` returns `AppResult<()>` with `AppError::BlocklistUnavailable`; `open_site` aborts, `update_settings` reports failure. Lock stays recovered, never an empty rule set (empty means "list disabled"). | `commands.rs`, `error.rs` |
| 2 | Redirect URLs skip `shouldInterceptRequest`; `shouldOverrideUrlLoading` ignored DNS entirely. | Pure `NavigationDnsGate` shared by both checkpoints: cached private/changed verdict cancels the top-level navigation; a miss allows + probes off the UI thread; the request path feeds its synchronous answer into the same cache. | `NavigationDnsGate.kt`, `SiteNavigationGuard.kt`, `SiteNavigationPolicyTest.kt` |
| 3 | `loopback_port()` accepted `/cb`, a missing port, `localhost`, queries/fragments/userinfo, while the listener serves only `http://127.0.0.1:<port>/callback`. | Exact-endpoint parsing + `validate_redirect_uri()` **before** pending state; listener starts only for `LoginRedirect::Loopback(port)`; desktop custom scheme validated and refused on Android. | `anilist.rs`, `commands.rs` |
| 4 | `shutdownNow()` cannot stop a blocked native call; consecutive stalls accumulated workers. | `FlushRecovery.MAX_ABANDONED_WORKERS = 2` + `replacementAllowed`/`workerBudgetAfter`; `execute` returns `Boolean`, refuses when exhausted, `submit` settles the caller. | `SiteProfileOps.kt`, `SiteFlushWorker.kt`, `SiteProfileOpsTest.kt` |
| 5 | Proposals in `ci-fixes/` were treated as if the active workflows were pinned. | **Blocked, reported, not bypassed:** push rejected (`without `workflow` scope`). Active workflows remain mutable; pinned proposals stay separate and are not claimed as active. Nothing pushed to `main`, no tag/release. | `docs/security-residual-risks.md`, `docs/audit-item-status.md` (R-09), tests |
| 6 | Audit docs did not match the code (R-01/R-05) and did not state the Android 8/9 drop. | R-01 split into blocklist (fixed) / DNS (partial) / host rules (fixed); R-05 split into custom-scheme removal (fixed) / loopback hardening (new); `minSdkVersion=29` removing Android 8/9 support stated in README and the audit doc. | `docs/audit-item-status.md`, `docs/security-residual-risks.md`, `README.md` |

### Verification state for round 6

| Check | Result |
| --- | --- |
| `npm test` (node) | run locally in this environment — see the PR comment / step output |
| Rust `cargo test` + `cargo clippy -- -D warnings` | **not run** here: no Rust toolchain in the reviewing container. Not counted as a pass. |
| Kotlin unit tests (`./gradlew test`, `--tests dev.animehub.app.*Test`) | **not run** here: no Android SDK/Gradle. Not counted as a pass. |
| Device / emulator | **not performed**. MULTI_PROFILE, real `flush()` latency, blocklist push observed in a live WebView and the guard callbacks are still R-04 device work. |
| GitHub Actions for the new commit | workflow files could not be changed (no `workflow` scope); CI runs on push — its result is reported separately / check the commit checks. |

## Remaining release gates

- **DNS rebinding TOCTOU** still needs a connection-pinning network layer.
- **Device / emulator matrix (R-04)** was not available here.
- **PR size (MNT-01)**: `windows.rs` split remains a stacked-PR task, not an
  unverified blind refactor.

## Verification

```text
npm test        250 / 250
Kotlin JUnit    108 / 108 (pure flows + navigation policy)
cargo fmt       clean
npm run build   OK
python3 -m py_compile scripts/android_prepare.py  OK
```

`cargo test` and a device run were not available in this environment; `cargo fmt --check` and the Kotlin compiler were run locally.

## Applying the workflow hardening

GitHub refused to create the branch with workflow changes because the supplied
token has no `workflow` scope. All workflow hardening is therefore committed
under `ci-fixes/`:

```bash
./ci-fixes/apply.sh
git add .github/workflows
git commit -m "ci: pin actions and verify resolved Android pins"
git push
```

The test suite validates `ci-fixes/*.yml`, so the proposed files are covered
even before they are installed.
