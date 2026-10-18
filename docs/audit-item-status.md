# Audit item status — R-01 … R-12

One status per item, re-derived from the code on this branch (PR #27), not
carried over from an earlier round. Vocabulary: **fixed / partially fixed /
already fixed / duplicate / false positive / deferred / unresolved**.

## About the source reports

Two review reports were produced while this branch was being worked on and
**neither is in the repository**: they were kept outside the tree and were lost
when the workspace was re-cloned. The only audit document that ships in-repo is
[`audit-cookie-session-lifecycle.md`](./audit-cookie-session-lifecycle.md)
(cookie/session lifecycle; it carries no `R-*` ids).

Consequences, stated rather than papered over:

* The scope sentences below come from the round-12 review instructions. Every
  **status** is justified from the code as it stands now — each row cites files
  and lines you can open — but the *original* finding prose is not recoverable,
  so a row cannot quote it.
* Where the lost wording was the thing that decided the verdict (R-04, R-05),
  the item stays **unresolved** instead of being guessed at.
* `AH-01…AH-16`, `AH-P*` and `AH-G*` belong to those lost reports and are kept
  as separate identities. They are referenced here only where this branch's own
  code and tests already cite them (`AH-04 / R-07` in `web/windows.rs`,
  `AH-P8 / R-06` in `tests/storage_scope.test.js`); no `AH-*` verdict is
  restated from memory.

## Status table

| Item | Scope reviewed | Status | Evidence on this branch |
| --- | --- | --- | --- |
| **R-01** | Android network policy: the sub-resource host/scheme filter exists, but the user blocklist and DNS-rebinding protection do not | **partially fixed** | Request-level filter: `SiteNavigationGuard.shouldInterceptRequest` (`android-plugin/kotlin/dev/animehub/app/SiteNavigationGuard.kt:78`), which refuses cleartext, private/loopback hosts and disallowed schemes and returns an empty 403 (`:85` logs scheme, reason and frame only). What is missing on Android is documented, not claimed away: `docs/android-storage-isolation.md:269` ("Why no DNS on Android…a hostname that resolves to a private address **is not caught on Android**") and `docs/android-navigation.md`. The Rust blocklist/DNS checks (`web/session.rs`, `sites/url_policy.rs`) run on the desktop paths only; plumbing the user blocklist into the Android WebView is a separate deferred item. No full-network-isolation claim is made anywhere. |
| **R-02** | `get_site_cookies` / `set_site_cookies` removed from the IPC surface | **fixed** | Neither command is registered nor defined; the only occurrences left are the comments explaining the removal (`commands.rs:832`, `:1792`). Pinned by `lib.rs:598` `raw_cookie_jar_commands_are_gone_from_the_ipc_surface`, which builds both names at runtime and asserts they appear in neither `invoke_handler` nor `commands.rs` nor **any** frontend source; `lib.rs:637` `cookie_jar_storage_keys_still_have_a_production_user` keeps the legitimate jar (`cookies-<site_id>`, `CookieJar`) alive for the 0.3.4 import and the mobile open/clear/delete protocols. |
| **R-03** | Shipped texts still described the 0.3.4 cookie-jar swap and an unsolved same-origin storage problem, while another section described per-site profiles | **fixed** (this round) | `README.md:8` (tagline), `:66` (security policy scope, now naming the Kotlin checkpoints and the shared vector fixture), `:117` (architecture table row), the encrypted-store and snapshot paragraphs; `src-tauri/tauri.conf.json` `bundle.longDescription` (this text ships inside the package metadata). Regression: `tests/gaps.test.js:37` carries the stale sentences **verbatim** as negative pins (whitespace-collapsed, so re-wrapping cannot hide them) plus positive pins for the profile model and the stated Android limits; reverting either file to the old wording was verified to fail that test. |
| **R-04** | Android behaviour verified only by compilation and host unit tests | **unresolved** | Nothing in this branch changes that. `docs/android-storage-isolation.md:847` ("Verification status (honest)") records what CI does prove (Kotlin unit tests over real flow code with fake ports, an aarch64 debug APK build) and what it does not, followed by the device checklist that remains: MULTI_PROFILE present/absent on two provider versions, cookie non-mixing across sites, default-profile data untouched, back-button ladder, PiP handoff, one-time migration, deferred-deletion draining, real `flush()` latency and watchdog paths, kill-between-phases, late-callback races. **No physical device or emulator has run this build.** |
| **R-05** | Android OAuth callback can be taken by another app that registers the same custom scheme | **unresolved** | This round hardened the *validation* of a callback we do receive — `anilist.rs:213` `parse_callback` now compares scheme, host, port (including an explicit port on the custom scheme), path, and rejects userinfo and fragments, with tests at `:1458`, `:1475`, `:1518`. That does **not** stop another app from receiving the OS intent for `animehub://anilist/callback` (`tauri.conf.json` `plugins.deep-link.mobile`, `appLink: false`). AniList's documented flow offers no PKCE and no app-link/asset-statement support this repo could verify, so none was invented; `README.md:273` says so explicitly. Stays an open pre-merge risk. |
| **R-06** | Is the Android HTTP-cache / Service-Worker limit shown to the user? | **fixed** | Shown in the UI, not only in docs: `src/logic/storage_scope.js:43` and `:48` build the clear-result message ("Android'de HTTP önbelleği ve servis çalışanları profil bazında silinemiyor; site önbellekten içerik gösterebilir."), imported by `src/main.js:39` and pinned by `tests/storage_scope.test.js:49` and `:66`. `docs/android-storage-isolation.md:237` states the same limit next to `cacheMode = LOAD_DEFAULT`. |
| **R-07** | Profile deletion: the tombstone was lifted without securing the generation counter | **fixed** (this round, host-tested) | Order is now tombstone → provider delete → **secure the generation** → forget → retire, in both paths: `web/windows.rs:1503` `secure_tombstone_generation` and `:1555` `run_pending_delete_drain` (startup), plus `run_mobile_delete_protocol`; `lib.rs:332` `drain_pending_profile_deletes` is a thin adapter over them. `:974` `generation_past_name` is monotone, idempotent, re-derives the profile name to prove ownership (never trusts parsed digits) and refuses at `u32::MAX` while keeping the durable tombstone; `:1025` `match_queued_name_to_site` requires exactly one owner, and an unmatchable tombstone is **kept**, logged and reported (`DrainReport::kept_unmatched`) instead of being dropped. Queue entries carry their owner (`PendingOwner::{Site,Orphan,Unknown}`) with legacy string-list queues and corrupt-queue salvage preserved. Behavioural tests drive the real protocol against fakes at every kill point (`windows.rs` test module, 17 added cases, incl. "every kill point leaves the next open with a different name", a failing generation write, and repeated drains not consuming a second generation); `tests/android_isolation.test.js:400` and `:561` pin the order from the JS side; `docs/android-storage-isolation.md:554` and `:569` describe it. **What a host test cannot prove** — when a real provider finishes `deleteProfile` — is still device work (R-04). |
| **R-08** | File/content access and popup/permission decisions must be explicit | **fixed** | Every decision is data in one tested plan: `SiteWebViewSettings.kt:43-95` — `allowFileAccess=false`, `allowContentAccess=false`, `allowFileAccessFromFileURLs=false`, `allowUniversalAccessFromFileURLs=false`, `mixedContentMode=NEVER_ALLOW` (the constant is pinned against the platform value), `geolocationEnabled=false` (no consent flow exists, so it is denied explicitly rather than left to a provider default), `supportMultipleWindows=false` with `javaScriptCanOpenWindowsAutomatically=true` — i.e. a popup becomes an ordinary same-view navigation that `SiteNavigationPolicy` judges — and `safeBrowsingEnabled=true`. `applySiteSettings(WebSettingsSink(...))` is the only code touching the platform object; `SiteWebViewSettingsTest` asserts the plan and that the adapter writes exactly these fields. |
| **R-09** | CI token scope, Action pinning, credential settings | **partially fixed** | Fixed: no workflow-wide write token — `.github/workflows/ci.yml:22` sets `permissions: contents: read` and the PR-comment dump is gone (diagnostics go to the step summary and log artifacts), pinned by `tests/gaps.test.js:724`. **Not** fixed: third-party Actions are referenced by mutable tag, not by commit SHA (`actions/checkout@v7`, `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache@v2`, `actions/setup-node@v7`, `actions/setup-java@v6`, `android-actions/setup-android@v4`, `actions/upload-artifact@v7`). Repository-level "required credential"/default-permission settings could not be read from this environment (`GET /repos/…/actions/permissions/workflow` → 403 for the available token), so that half is **unverified here** and needs an owner with admin access. |
| **R-10** | Do both policy suites really use the same fixture? | **fixed** | One file, two loaders, and the wiring is pinned: Rust reads it at compile time (`sites/url_policy.rs:908`, `include_str!("../../../tests/fixtures/navigation-policy.tsv")`); CI copies the *same* path into the Android test resources and hard-fails if the copy fails (`.github/workflows/ci.yml:242`); Kotlin reads it from the classpath (`SiteNavigationPolicyTest.kt:295`, and `:257` documents the copy) and throws if it is missing rather than silently shrinking the suite. `tests/gaps.test.js:165` and `tests/android_isolation.test.js:1285` assert the copy step and the vector count from the JS side. Known divergence, documented not hidden: Kotlin has no `MISSING_HOST` reason code, so parity is claimed at vector level over the shared file. |
| **R-11** | Top-level `data:` rejection and the `blob:` video allowance | **fixed** (evaluated separately) | `data:` as a top-level document is **refused** (`sites/url_policy.rs:192` → `NavBlock::DataNavigation`): an opaque-origin HTML/script carrier no site legitimately needs, while the app's own `data:image/…` logos are sub-resources and never reach that check (test at `:954`). `blob:` stays **allowed on purpose** and is restricted rather than blanket-accepted: `:183` requires the blob's inner origin to be an allowed https origin, so `blob:https://openani.me/…` passes (test `:975`) and `blob:http://…` is rejected (`:978`). MSE players navigate the main frame to `blob:` object URLs, so blocking it would break playback — the two halves are judged on their own merits and both are tested. |
| **R-12** | Log redaction and CI comment cleanup | **fixed** (the review itself is a separate item) | Redaction: the in-app log keeps areas and short messages and "never message bodies or cookie values" (`AnimeHubDebugLog.kt:17`, capped at 200 lines); the navigation log records scheme, reason and frame only (`SiteNavigationGuard.kt:85`), with the rule in pure code so it is assertable (`SiteNavigationPolicy.kt:140`) and asserted (`tests/android_isolation.test.js:1265`). CI: no PR-comment dump and no exported write token (`ci.yml:22`, pinned by `tests/gaps.test.js:724`), no `|| true` / `continue-on-error` masking (`tests/gaps.test.js:162`, `:172`, `:183`). The two things R-12 must **not** be read as closing — an independent human review, and a device/emulator matrix — are listed below and in R-04. |

## Not closed by any of the above

These stay open pre-merge risks; documenting them is not closing them.

1. **Android DNS rebinding / private-resolving hosts and the user blocklist.**
   A hostname that resolves to a private address is not caught on Android, and
   the user's blocklist is not enforced inside the site WebView (R-01).
2. **Android OAuth custom-scheme hijack.** Another app can register
   `animehub://anilist/callback` and receive the intent; the hardened
   exact-match check only validates callbacks this app actually receives (R-05).
3. **No device or emulator testing.** Every Android claim on this branch is
   backed by compilation and host-side unit tests only (R-04).
4. **No independent human review.** All review so far is agent review of the
   same diff; that is not a substitute (R-12's explicit exclusion).
5. **Every-origin document-start script (AH-P2).** The origin rule set stays
   `setOf("*")`. What changed is that no frame receives a capability: the popup
   guard keeps no reference to the native `window.open`, and the PiP controller
   installs in the top frame only. Recorded as **partially fixed** with the
   per-frame table in
   [`android-storage-isolation.md`](./android-storage-isolation.md#injected-script-origin-and-frame-scope-ah-p2)
   — and host-tested only.
6. **Third-party Actions pinned by tag, not SHA**, and repository-level
   credential settings unverified from this environment (R-09).
