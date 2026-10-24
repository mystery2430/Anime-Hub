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
* Where the lost wording was the thing that decided the verdict (R-04), the
  item stays **unresolved** instead of being guessed at. An earlier round listed
  R-05 here as well while its own row below said **fixed** — a direct
  contradiction. It is resolved in favour of the code, because R-05's verdict
  does not actually depend on the lost wording: `tauri.conf.json` ships
  `plugins.deep-link.mobile: []`, and `commands::anilist_login_start` refuses a
  custom-scheme redirect on Android (`mobile_native_login_allowed`, then
  `validate_redirect_uri(uri, cfg!(desktop))`) **before** any pending state is
  written. R-05 stays **fixed** for Android; see "Not closed" #2 for the narrow
  fact that remains true.
* `AH-01…AH-16`, `AH-P*` and `AH-G*` belong to those lost reports and are kept
  as separate identities. They are referenced here only where this branch's own
  code and tests already cite them (`AH-04 / R-07` in `web/windows.rs`,
  `AH-P8 / R-06` in `tests/storage_scope.test.js`); no `AH-*` verdict is
  restated from memory.

## Status table

| Item | Scope reviewed | Status | Evidence on this branch |
| --- | --- | --- | --- |
| **R-01** | Android network policy: user blocklist, private-host rules and DNS-rebinding protection | **partially fixed** — (a)/(c) fixed, **(b) DNS closed by design on Android, still OPEN pending device verification** | **(a) Blocklist plumbing — fixed.** `Blocklist::entries()` serialises the rules; `commands::sync_android_blocklist(app, state)` pushes them before every open and after every settings change **and reset** (`reset_blocklist` previously returned without a push). Fail-closed now covers an already-open WebView: `transfer_blocklist_snapshot` pushes first, then closes the active site through `close_site_window` (`ANDROID_SESSION_LOCK`, flush, `current_site` release) before returning `AppError::BlocklistUnavailable`; a failed close is a restart-required `Other` error, never `Ok`.
  **Transfer ordering (new, separate):** `sync_android_blocklist` is called by
  `open_site`, `update_settings` and `reset_blocklist` *outside*
  `ANDROID_SESSION_LOCK` (that lock guards the site lifecycle, and the
  fail-closed close has to take it), and Kotlin's `SiteBlocklist.update` is a
  last-writer-wins atomic swap — so a transfer built from older settings could
  previously land *after* a newer one and silently restore the old rules, with
  the settings file, the Rust matcher and the Kotlin snapshot disagreeing and no
  error anywhere. `commands::transfer_blocklist_with_generation` now serializes
  transfers on `BlocklistSyncGate::lock`, **re-reads** the snapshot inside that
  lock (`blocklist_transfer_snapshot`) and skips any generation already
  delivered (`applied`), which is bumped by `rebuild_blocklist` while the
  matcher lock is held. `applied` advances only after a confirmed push, so a
  failure is retried rather than believed. Lock order is
  `BlocklistSyncGate::lock` → `ANDROID_SESSION_LOCK`; nothing takes them the
  other way round. Rust tests: `first_blocklist_transfer_pushes_even_before_any_rebuild`,
  `stale_blocklist_snapshot_is_refused_without_closing_a_live_site`,
  `delayed_transfer_delivers_the_newest_snapshot_not_the_captured_one`,
  `blocklist_push_failure_closes_the_live_site_and_does_not_advance_applied`,
  `blocklist_close_failure_is_restart_required_and_leaves_applied_behind`,
  `rebuild_blocklist_bumps_the_generation_once_per_rebuild`,
  `disabled_blocklist_still_transfers_an_empty_snapshot_through_the_guard`,
  `reset_blocklist_reaches_android_with_the_default_snapshot`,
  `concurrent_out_of_order_transfers_never_restore_a_stale_snapshot`. Kotlin: `SiteBlocklist.kt` is an atomic process-wide snapshot consumed by `SiteNavigationPolicy.hostAllowed` at both checkpoints. Rust tests `blocklist_transfer_success_does_not_close_an_open_site`, `blocklist_transfer_closes_the_live_site_before_returning_push_error`, `blocklist_transfer_close_failure_requires_a_restart_not_success`; the Kotlin suite pins replacement/revocation. **(b) DNS — mitigated by construction on Android, still OPEN pending a device run.** Round 16 moved the decision to connect time: an in-process loopback proxy (`DnsPinnedProxy.kt`) is installed process-wide through androidx.webkit `ProxyController` with exactly one `addProxyRule` plus `removeImplicitRules()` — no `addDirect`, no bypass rule, and never cleared — and the first `loadUrl` of a site is gated on the install callback (5 s watchdog). It accepts `CONNECT` only, parses the authority strictly, resolves the name **once**, classifies every answer address with the shared `IpPolicy` (mixed, empty, throwing, private/loopback/link-local/ULA/mapped → refuse), then connects with `InetSocketAddress(address, port)` — no second lookup — and verifies `socket.inetAddress` before tunnelling bytes; TLS is never terminated and certificate errors are never swallowed. Unsupported provider, bind failure, thrown exception or a missing callback all refuse the open with `proxy-unavailable` (user text in `android_bridge.rs`), never `ALLOW_AND_PROBE` and never a direct fallback. `NavigationDnsGate` stays, unchanged, as **defence in depth**, not the guarantee. Proven by real-socket JVM tests (`DnsPinnedProxyTest`, `SiteProxyGateTest`), Rust/JS source-contract tests and the Android compile job; **not observed on a device** — the WebView↔proxy path, QUIC suppression, redirect hops and launcher/IPC behaviour are the open items in `docs/android-dns-pinning-device-checklist.md`. Desktop keeps the old pre-check and its TOCTOU window. **Scope of the pin, stated narrowly (round 16.1):** the pin covers HTTP(S)/WSS over TCP `CONNECT`. **WebRTC/ICE/STUN/TURN (UDP) is NOT pinned** — Android WebView cannot be told to force it through a proxy (RFC 8828 §5.1 principle 4, no WebView policy API); the Android init script removes `RTCPeerConnection` as defence in depth, which is not a boundary. The loopback listener is reachable by other apps on the device (shared loopback; the platform security guidance says so), so it is an unauthenticated public-egress listener: bounded by a 16 connection ceiling, a handshake deadline and public-destination-only enforcement, but NOT authenticated — still open. Resolver timeouts, thread bounding and socket-close on `stop()` are fixed and measured by real-socket tests (round 16.4 additionally makes `start()` all-or-nothing: `Limits` is validated before any socket exists and every failure path releases the listener, both pools, the accept thread and the resolver generation, so a failed start leaves `port == null`; round 16.3 adds generation reuse and a budget that spans `stop()`/`start()`, so an idle older generation is used instead of refusing and a restart cannot spend the cap twice) with an **uninterruptible** fake resolver: one spare generation (`resolveThreads = 4` per generation, `resolveThreadCap = 8`) plus `RESOLVE_EXHAUSTED` beyond the ceiling. The cap is an availability residual, not automatic recovery: a platform call that never returns still wedges DNS until the process restarts, and the budget deliberately spans `stop()`/`start()` because those threads survive both (`docs/security-residual-risks.md` round 16.3). **(c) Built-in host/IP rules — fixed**, shared vectors in `tests/fixtures/navigation-policy.tsv`. |
| **R-02** | `get_site_cookies` / `set_site_cookies` removed from the IPC surface | **fixed** | Neither command is registered nor defined; the only occurrences left are the comments explaining the removal (`commands.rs:832`, `:1792`). Pinned by `lib.rs:598` `raw_cookie_jar_commands_are_gone_from_the_ipc_surface`, which builds both names at runtime and asserts they appear in neither `invoke_handler` nor `commands.rs` nor **any** frontend source; `lib.rs:637` `cookie_jar_storage_keys_still_have_a_production_user` keeps the legitimate jar (`cookies-<site_id>`, `CookieJar`) alive for the 0.3.4 import and the mobile open/clear/delete protocols. |
| **R-03** | Shipped texts still described the 0.3.4 cookie-jar swap and an unsolved same-origin storage problem, while another section described per-site profiles | **fixed** (this round) | `README.md:8` (tagline), `:66` (security policy scope, now naming the Kotlin checkpoints and the shared vector fixture), `:117` (architecture table row), the encrypted-store and snapshot paragraphs; `src-tauri/tauri.conf.json` `bundle.longDescription` (this text ships inside the package metadata). Regression: `tests/gaps.test.js:37` carries the stale sentences **verbatim** as negative pins (whitespace-collapsed, so re-wrapping cannot hide them) plus positive pins for the profile model and the stated Android limits; reverting either file to the old wording was verified to fail that test. |
| **R-04** | Android behaviour verified only by compilation and host unit tests | **unresolved** | Nothing in this branch changes that; round 16 adds the network matrix in `docs/android-dns-pinning-device-checklist.md` (proxy support and refusal, applied-while-launcher-works, rebinding capture, no direct egress, redirects/iframes/WSS/QUIC, PiP/back, lifecycle), which has **not** been run. `docs/android-storage-isolation.md:847` ("Verification status (honest)") records what CI does prove (Kotlin unit tests over real flow code with fake ports, an aarch64 debug APK build) and what it does not, followed by the device checklist that remains: MULTI_PROFILE present/absent on two provider versions, cookie non-mixing across sites, default-profile data untouched, back-button ladder, PiP handoff, one-time migration, deferred-deletion draining, real `flush()` latency and watchdog paths, kill-between-phases, late-callback races. **No physical device or emulator has run this build.** |
| **R-05** | Android OAuth callback can be taken by another app that registers the same custom scheme | **fixed** (custom-scheme refusal + loopback-only login) | `tauri.conf.json` sets `plugins.deep-link.mobile: []`, and `anilist::validate_redirect_uri(uri, allow_custom_scheme)` is called in `anilist_login_start` **before** any pending state is written (custom scheme → `AppError::AniList` on Android). **Loopback hardening (new, separate):** `loopback_port()` now accepts exactly the endpoint `LoopbackListener` serves — `http://127.0.0.1:<explicit port>/callback`, no query/fragment/userinfo, no `localhost`/`[::1]` — so `/cb`, a missing port or a foreign port can no longer start a login that cannot finish; the listener starts only for `LoginRedirect::Loopback(port)`. Token handling itself (`state` single-use, constant-time compare) is unchanged. **Correction (round 16.4):** the built Android manifest still declares the `animehub` filter — `scripts/android_prepare.py` injects it and `tests/gaps.test.js` pins it — so the fix is the refusal above, not the absence of a filter. The filter is inert: no OAuth redirect is ever sent to that scheme on Android, and an unsolicited intent is parsed against the loopback redirect and rejected.
  **Scope of this verdict — Android only, and it is a statement about *this*
  app.** Two facts must not be conflated: (i) any app can still *register*
  `animehub://` with the OS — that is Android's behaviour and this branch cannot
  change it; (ii) this app no longer *uses* that scheme on Android, so there is
  no OAuth intent, code or `state` for a hijacker to receive. What (i) leaves
  open is only scheme squatting against a URI this app never sends to.
  **Desktop is different and is not covered by this row:**
  `plugins.deep-link.desktop.schemes` still contains `animehub`, and
  `validate_redirect_uri` is called with `cfg!(desktop)`, i.e.
  `allow_custom_scheme = true` there. The desktop exact-host/path check
  (`animehub://anilist/callback`, no other host or path) is unchanged and is a
  separate, lower-severity surface. |
| **R-06** | Is the Android HTTP-cache / Service-Worker limit shown to the user? | **fixed** | Shown in the UI, not only in docs: `src/logic/storage_scope.js:43` and `:48` build the clear-result message ("Android'de HTTP önbelleği ve servis çalışanları profil bazında silinemiyor; site önbellekten içerik gösterebilir."), imported by `src/main.js:39` and pinned by `tests/storage_scope.test.js:49` and `:66`. `docs/android-storage-isolation.md:237` states the same limit next to `cacheMode = LOAD_DEFAULT`. |
| **R-07** | Profile deletion: the tombstone was lifted without securing the generation counter | **fixed** (this round, host-tested) | Order is now tombstone → provider delete → **secure the generation** → forget → retire, in both paths: `web/windows.rs:1503` `secure_tombstone_generation` and `:1555` `run_pending_delete_drain` (startup), plus `run_mobile_delete_protocol`; `lib.rs:332` `drain_pending_profile_deletes` is a thin adapter over them. `:974` `generation_past_name` is monotone, idempotent, re-derives the profile name to prove ownership (never trusts parsed digits) and refuses at `u32::MAX` while keeping the durable tombstone; `:1025` `match_queued_name_to_site` requires exactly one owner, and an unmatchable tombstone is **kept**, logged and reported (`DrainReport::kept_unmatched`) instead of being dropped. Queue entries carry their owner (`PendingOwner::{Site,Orphan,Unknown}`) with legacy string-list queues and corrupt-queue salvage preserved. Behavioural tests drive the real protocol against fakes at every kill point (`windows.rs` test module, 17 added cases, incl. "every kill point leaves the next open with a different name", a failing generation write, and repeated drains not consuming a second generation); `tests/android_isolation.test.js:400` and `:561` pin the order from the JS side; `docs/android-storage-isolation.md:554` and `:569` describe it. **What a host test cannot prove** — when a real provider finishes `deleteProfile` — is still device work (R-04). |
| **R-08** | File/content access and popup/permission decisions must be explicit | **fixed** | Every decision is data in one tested plan: `SiteWebViewSettings.kt:43-95` — `allowFileAccess=false`, `allowContentAccess=false`, `allowFileAccessFromFileURLs=false`, `allowUniversalAccessFromFileURLs=false`, `mixedContentMode=NEVER_ALLOW` (the constant is pinned against the platform value), `geolocationEnabled=false` (no consent flow exists, so it is denied explicitly rather than left to a provider default), `supportMultipleWindows=false` with `javaScriptCanOpenWindowsAutomatically=true` — i.e. a popup becomes an ordinary same-view navigation that `SiteNavigationPolicy` judges — and `safeBrowsingEnabled=true`. `applySiteSettings(WebSettingsSink(...))` is the only code touching the platform object; `SiteWebViewSettingsTest` asserts the plan and that the adapter writes exactly these fields. |
| **R-09** | CI token scope and Action pinning | **partially fixed — Action pinning implemented; CI pending** | All four active `.github/workflows/*.yml` files now pin 36 external Action references to the verified 40-character commit SHAs in `docs/security-residual-risks.md`; each keeps a human-readable former-tag comment. The two in-repo reusable workflow references remain relative paths. The changes touch only these 36 `uses:` lines, preserving triggers, permissions, signing environment, release gates and artifact steps. `tests/gaps.test.js` and `tests/command_wiring.test.js` now validate active workflows rather than only `ci-fixes/`. The old missing-`workflow`-scope push failure is no longer blocking this PR-branch edit; no credentials or permissions were changed and `main`, tags and releases were not touched. CI must pass before merge. The separate inability to trigger `workflow_dispatch` from the integration remains a tooling limitation, not the action-pinning vulnerability. |
| **R-10** | Do both policy suites really use the same fixture? | **fixed** | One file, two loaders, and the wiring is pinned: Rust reads it at compile time (`sites/url_policy.rs:908`, `include_str!("../../../tests/fixtures/navigation-policy.tsv")`); CI copies the *same* path into the Android test resources and hard-fails if the copy fails (`.github/workflows/ci.yml:242`); Kotlin reads it from the classpath (`SiteNavigationPolicyTest.kt:295`, and `:257` documents the copy) and throws if it is missing rather than silently shrinking the suite. `tests/gaps.test.js:165` and `tests/android_isolation.test.js:1285` assert the copy step and the vector count from the JS side. Known divergence, documented not hidden: Kotlin has no `MISSING_HOST` reason code, so parity is claimed at vector level over the shared file. |
| **R-11** | Top-level `data:` rejection and the `blob:` video allowance | **fixed** (evaluated separately) | `data:` as a top-level document is **refused** (`sites/url_policy.rs:192` → `NavBlock::DataNavigation`): an opaque-origin HTML/script carrier no site legitimately needs, while the app's own `data:image/…` logos are sub-resources and never reach that check (test at `:954`). `blob:` stays **allowed on purpose** and is restricted rather than blanket-accepted: `:183` requires the blob's inner origin to be an allowed https origin, so `blob:https://openani.me/…` passes (test `:975`) and `blob:http://…` is rejected (`:978`). MSE players navigate the main frame to `blob:` object URLs, so blocking it would break playback — the two halves are judged on their own merits and both are tested. |
| **R-12** | Log redaction and CI comment cleanup | **fixed** (the review itself is a separate item) | Diagnostics: Rust and Kotlin now keep separate in-memory structured rings capped at 2,000 events each; the developer panel merges them into a time/source-ordered 4,000-event view, reports dropped events, and exposes only allowlisted codes/counts through a second JavaScript projection. The operation IDs are opaque `r-N`/`k-N`; no URL, host, cookie, token, password, AniList secret, origin, profile name, page/storage content or raw exception text is admitted. Regression coverage is in `src-tauri/src/diagnostics.rs`, `AnimeHubDebugLogTest.kt`, `SiteProfileOpsTest.kt`, and `tests/debug_log.test.js`; navigation still records no per-request/page details (`SiteNavigationGuard.kt`). CI: no PR-comment dump and no exported write token (`ci.yml`, pinned by `tests/gaps.test.js`), no `|| true` / `continue-on-error` masking. The two things R-12 must **not** be read as closing — an independent human review, and a device/emulator matrix — are listed below and in R-04. |

## Not closed by any of the above

These stay open pre-merge risks; documenting them is not closing them.

1. **Android DNS rebinding / private-resolving hosts.** The user blocklist IS
   transferred to the Android site WebView, and a failed transfer now aborts the
   open/closes a live view (see R-01 (a)). DNS is different: a cache miss at
   `shouldOverrideUrlLoading` returns `ALLOW_AND_PROBE`, the engine re-resolves
   at connect time, and there is no network-layer pinning. A hostname can still
   answer public to the pre-check and private to the socket. This remains an
   **open** residual risk (R-01 (b)); a test or comment does not close it.
2. **`animehub://` scheme squatting — not an OAuth hijack on Android.**
   Corrected: an earlier round described this as "another app can register
   `animehub://anilist/callback` **and receive the intent**", which contradicted
   the R-05 row and `docs/security-residual-risks.md`. There is no OAuth intent
   to receive, because `anilist_login_start` refuses a custom-scheme redirect
   before any pending state exists (`mobile_native_login_allowed` +
   `validate_redirect_uri(uri, cfg!(desktop))`) — not because the app declares
   no filter: `scripts/android_prepare.py` still injects the `animehub` filter
   into every generated manifest, and nothing sends a redirect to it. What genuinely remains is
   narrower: a third party can still register the scheme at OS level, and
   **desktop** still uses it (`deep-link.desktop.schemes: ["animehub"]`,
   `allow_custom_scheme = cfg!(desktop)`), where the exact host+path check only
   validates callbacks this app actually receives. Do not read either fact as an
   Android OAuth hijack (R-05).
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
6. **Third-party Actions pinned by tag, not SHA — BLOCKED on token
   permission**, and repository-level credential settings unverified from this
   environment. Re-attempted and refused again this round; the verified SHA
   table and the exact change a maintainer must make are in
   `docs/security-residual-risks.md` (R-09).

## Round 6 deltas (re-derived from this branch's code)

| Change | Status | Where |
| --- | --- | --- |
| Android blocklist transfer is fail-closed | fixed | `commands.rs` `sync_android_blocklist` → `AppResult<()>`; `open_site`/`update_settings` propagate; new `AppError::BlocklistUnavailable` |
| Disabled blocklist no longer blocks on Android | fixed | `android_blocklist_entries` (empty snapshot when `!is_enabled()`) |
| Redirect/main-frame DNS verdicts | **partial** | `NavigationDnsGate.kt` + `SiteNavigationGuard.kt`; TOCTOU unchanged |
| Loopback redirect must match the served endpoint | fixed | `anilist.rs` `loopback_port`, `validate_redirect_uri`, `LoginRedirect` |
| Redirect validated before pending OAuth state | fixed | `commands.rs` `anilist_login_start` |
| Flush worker replacement is bounded | fixed (host-tested; device behaviour still R-04) | `SiteProfileOps.kt` `MAX_ABANDONED_WORKERS`, `SiteFlushWorker.kt` refusal |
| Blocklist transfers serialized + generation-guarded | fixed (host-tested; `cargo test` not runnable in the review environment — see R-04) | `commands.rs` `BlocklistSyncGate`, `transfer_blocklist_with_generation`, `blocklist_transfer_snapshot`, `rebuild_blocklist` |
| Active workflows pinned to commit SHAs | **implemented on PR branch; CI pending** | see R-09 |

### Android 8/9 support was dropped

`minSdkVersion` is **29 (Android 10)** because `MULTI_PROFILE` needs the
multi-process WebView baseline. Devices on **Android 8.x and 9** are therefore
no longer supported by this branch at all: the APK does not install, and a
provider without `MULTI_PROFILE` makes the app refuse the open with guidance
instead of falling back to a shared profile. That is a deliberate scope
decision (storage isolation over OS coverage), not an incidental build change.
