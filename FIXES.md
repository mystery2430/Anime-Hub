# PR #27 review fixes

Based on `mystery2430/Anime-Hub` PR #27 head
`9bf35ffc9e7223e305bfaa9c68f2f8ff288e844e`.

| Review id | Fix | Evidence |
| --- | --- | --- |
| SEC-01 / R-05 | Android custom-scheme OAuth filter removed; native login is loopback-only and refuses a custom scheme before writing pending state. | `tauri.conf.json`, `anilist.rs`, `commands.rs`, `lib.rs` |
| BUG-01 | Legacy jar migration filters to host-ownable cookies; individual provider rejections are skipped, not fatal. | `web/windows.rs`, `SiteMigrationFlow.kt` |
| BUG-02 | `minSdkVersion` 29 with Android 10 / updated WebView guidance. | `tauri.conf.json`, `android_bridge.rs` |
| BUG-03 | Stuck flush worker is replaced after a measured stall. | `SiteFlushWorker.kt`, `SiteProfileOps.kt` |
| SEC-02 (partial) | DNS pre-check refuses private answers and changed public answers; TOCTOU documented. | `SiteNavigationGuard.kt`, `SiteNavigationPolicy.kt`, `docs/security-residual-risks.md` |
| R-01 | Android user blocklist is pushed from Rust before every open and enforced in navigation + sub-resources. | `sites/blocklist.rs`, `commands.rs`, `SiteBlocklist.kt`, `AnimeHubPlugin.kt` |
| DEP-01 | Every external Action pinned to a commit SHA, guarded by a test. The hardened files are in `ci-fixes/` (the push token has no `workflow` scope) and installed by `ci-fixes/apply.sh`. | `ci-fixes/*.yml`, `tests/gaps.test.js` |
| DEP-02 | `android_prepare.py` fails on duplicated/missing pins; the proposed CI checks the resolved Gradle graph. | `scripts/android_prepare.py`, `ci-fixes/ci.yml` |
| MNT-03 | Capability description matches the native per-site WebView model. | `capabilities/default.json` |
| VER-01 / MNT-02 | Behavioral tests added for migration, flush recovery, DNS verdicts, blocklist matching and action pinning. | test suites |

## Remaining release gates

- **DNS rebinding TOCTOU** still needs a connection-pinning network layer.
- **Device / emulator matrix (R-04)** was not available here.
- **PR size (MNT-01)**: `windows.rs` split remains a stacked-PR task, not an
  unverified blind refactor.

## Verification

```text
npm test        249 / 249
Kotlin JUnit    107 / 107 (pure flows + navigation policy)
cargo fmt       clean
npm run build   OK
python3 -m py_compile scripts/android_prepare.py  OK
```

`cargo test` and a device run were not available in this environment.

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
