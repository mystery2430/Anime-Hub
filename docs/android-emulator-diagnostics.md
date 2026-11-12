# Android end-to-end diagnostics and emulator test plan

**Status: source audit and actionable plan, not a test result.** This document
maps the current Round 15 working tree. No real emulator/provider run or CI run
for this working tree has been verified. Treat all emulator flows below as
planned until someone records the build, device/provider version, observed
behavior, and safe event excerpt.

## 1. What the developer panel records

The panel is in **Settings → Geliştirici seçenekleri**. Collection and reads
are gated by the persisted `developer_options` setting. The Rust command
checks that setting before reading the Kotlin ring; disabling the setting
turns off both collectors and clears their in-memory events. Rust serializes a
gate-off with panel reads and preflights Kotlin disable before persisting the
setting, avoiding a stale Kotlin collector after the saved option is off.

| Property | Behavior |
| --- | --- |
| Storage | Process-local memory only; no automatic file write and no network upload. |
| Bounds | 2,000 Rust events + 2,000 Kotlin events; combined snapshot at most 4,000. Each ring drops its oldest row on overflow and counts dropped rows; the panel reports the combined count. |
| Schema | `sequence`, `sourceSequence`, `timestampMs`, `source`, `area`, `phase`, `result`, and optional `operationId`, `durationMs`, `watchdog`, `errorClass`, `count`, `stepId`. Text values come from closed allowlists. |
| Ordering | Each ring uses a monotone sequence and non-decreasing wall-clock milliseconds. Merge sorts by timestamp, then Rust before Kotlin for timestamp ties, then per-source sequence; the UI assigns merged sequence numbers. This is useful timeline order, not a globally causal clock. |
| Correlation | Opaque `r-N` / `k-N` IDs are validated before export. Rust passes an operation ID across the Android bridge; Kotlin callbacks retain it. Not every app-start/lifecycle event has an operation ID. |
| Reset/export | **Sıfırla**, **Copy**, and **JSON indir** are explicit user actions. Reset clears both rings and leaves a safe reset marker. The downloaded JSON exists only because the user pressed the download button. |

No request/page/navigation URL or page content is logged. Cookie migration can
report a count and numbered step, not a cookie value. Profile enumeration and
geolocation can report counts, not names or origins. `cookie_write` in the Rust
clear protocol means deletion of encrypted per-site session/migration data; it
is not a provider cookie write. There is no `cookie_verify` event in the
current clear flow: successful completion is represented by
`clear/verified/verified` from Kotlin and `clear/complete` from both protocol
layers.

### Ordering and correlation limitations

- Rust's current-operation scope is thread-local and covers synchronous
  protocol bodies. Async open establishes its Rust operation scope after the
  DNS await; the Rust ID is then passed to Android. It is not a span tree and
  does not cross arbitrary async/thread boundaries.
- Back-to-launcher passes its Kotlin ID through the close command. Open,
  migration, clear, delete, and bridge calls share IDs where the call path
  carries one. Independent app/lifecycle/capability events may be uncorrelated.
- IDs and sequence counters restart with the process. Keep pre- and
  post-restart exports separate; the same short ID can occur in different
  processes.
- Several layers legitimately report an operation: a Rust protocol phase, a
  `bridge/<operation>` boundary, and a Kotlin provider/UI phase. Do not expect
  one row per logical phase across sources. `bridge` is a distinct Rust area
  so the plugin boundary is recognizable. The clear and migration Kotlin
  flows were adjusted to avoid redundant controller-level start/complete and
  geolocation-wipe rows; phase counts can still repeat across Rust/Kotlin.
- A `poll/pending` or `launcher_close/pending` row is not proof that close
  completed. Look for the terminal `closed`, `failed`, `fallback`, or `timeout`
  decision and the corresponding close result.

## 2. Source audit

### Components and what they contribute

| Source | Role in the timeline |
| --- | --- |
| `src-tauri/src/diagnostics.rs` | Rust bounded ring, operation scopes, Rust/Kotlin merge, closed event schema, error classification, drop count and snapshot capacity. |
| `src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubDebugLog.kt` | Kotlin bounded ring; validates area/phase/result, operation IDs and optional safe codes; no exception-text field exists. |
| `src-tauri/src/android_bridge.rs` | Adds Rust `bridge/<phase>` start/result rows and safe duration/error class around Android plugin calls. Provider/plugin error strings are discarded for diagnostics and sanitized before returning relevant site-profile errors. |
| `AnimeHubPlugin.kt` | App/API-level and launcher IPC setup/capability; profile-list capability/count; manual PiP bridge outcomes; developer-log snapshot/reset bridge. |
| `AnimeHubSiteController.kt`, `SiteClearFlow.kt`, `SiteMigrationFlow.kt`, `SiteCloseFlow.kt`, `SiteShutdownFlow.kt`, `SiteFlushWorker.kt`, `AnimeHubPipController.kt`, `PipTransferFlow.kt` | Kotlin WebView/profile lifecycle, clear/migration/close/watchdog decisions, shutdown flush, Back ladder, PiP ownership and safe provider-result codes. |
| `src-tauri/src/web/windows.rs` | Rust open/migration/clear/delete/drain/orphan protocols; generation, tombstone, queue and quarantine retirement outcomes. The platform calls are behind host-testable protocol functions. |
| `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs` | Command operation scopes, DNS rejection classification, persisted developer-options gate, async startup bridge retries/drain and per-open coordination. |
| `src/main.js`, `src/logic/debug_log.js`, `src/logic/copy_text.js`, `index.html`, `src/styles.css` | Hidden-unless-enabled developer panel; second allowlist/sanitization boundary; summary, refresh, explicit copy/download/reset. |
| `scripts/android_prepare.py`, `.github/workflows/ci.yml` | Prepare generated Android sources/test files, compile Kotlin production flows, run Android unit tests, build the aarch64 debug APK and upload the artifact. |

### Logged event map

Names below are `area/phase`; the final result and optional fields depend on
branch outcome. The panel's `source` distinguishes Rust from Kotlin.

| Flow | Events to expect and interpretation |
| --- | --- |
| App startup / IPC | Kotlin `app/start/ok` includes Android API level as a count. Kotlin `ipc/ipc_setup` is `ok`, `unsupported`, `unavailable`, or `failed_closed`. Rust `startup/start` begins setup; `startup/capability` reports developer-log bridge application where applicable. These app-wide events may have no operation ID. |
| Startup delete drain | Rust `queue/profile_list` reports entry count; `queue/resolve` reports blocked/owed/lifted/unknown counts. `startup/drain` is `started`, `empty`, `ok`, `unavailable`, `failed`, or `timeout`; `startup/worker_wait` records retry outcome and attempt count. A bridge-not-ready round retries; startup makes up to eight attempts 400 ms apart, then leaves durable work for another start. Empty queue is `startup/drain/empty` plus worker result `startup/worker_wait/ok`, not a promise that the UI is held behind a global drain barrier. Each open independently resolves queued names and persists an advance before migration/open under `ANDROID_SESSION_LOCK`. |
| Profile capability | Kotlin `profile/capability/supported` or `unsupported_provider`; Rust plugin-boundary rows use `bridge/capability` or `bridge/profile_list`. Enumeration reports a count only. Unsupported multi-profile is a feature gap; the app must refuse site open/clear/delete rather than fall back to the launcher's default profile. |
| Open / DNS / generation | Rust operation scope `open/start`; `commands.rs` can record `open/resolve/rejected` for a private-address DNS result or a safe resolver failure. Rust `open/profile_check`, `open/resolve`, `open/generation` and `open/complete` describe profile/tombstone resolution; `open/resolve` success can be `skipped` or `fallback`. Kotlin reports `open/start`, `profile/capability`, `profile/profile_check`, `open/view_create`, `open/profile_bind`, `open/view_attach`, `open/slot_claim`, `open/navigation_start`, `open/complete`. Android plugin boundary is `bridge/open`. No full URL/host is logged. |
| Migration | Rust `migration/migration` reports started/skipped and cookie count; on nonempty partial import fallback it also reports `migration/tombstone`, `migration/generation`, and `migration/migration/fallback`. `migration/flag_write` must be `ok` before Rust `open/open` begins. Kotlin phases include `migration/start`, `profile_check`, `profile_create`, numbered `cookie_write`, `flush`, `watchdog_arm`, `late_callback`, `complete`. Empty legacy jar may produce Kotlin `migration/complete/empty`; an existing flag makes Rust report migration skipped. `bridge/migration` measures the Android plugin call. No cookie contents are logged. |
| Quarantine / open gate | A timed-out uncancellable cookie delete records Kotlin `profile/quarantine/quarantined`; an eventual callback may record `profile/quarantine/settled`, but settlement alone does not reopen the profile. A successful wipe after all earlier operations settle records `clear/verified/verified`; controller may separately report `profile/quarantine/blocked`. Both Kotlin migration and open check the gate before provider work. |
| One-site clear | Rust operation scope `clear/start`, `clear/profile_check`, `clear/cookie_wipe`, `clear/cookie_write`, `clear/complete`; `cookie_write` removes encrypted per-site session/migration data after the provider profile wipe succeeds. Kotlin `clear/start` is emitted by `SiteClearFlow`; its phases include `capability`, `profile_check`, `cookie_wipe`, `geolocation_wipe`, `geolocation_origins` (count only), `storage`, `flush`, `watchdog_arm`, `late_callback`, `verified`, `complete`. `bridge/clear` measures the plugin call. `clear/verified/verified` is the successful verification marker. The controller's activity-gone/invalid/active-profile refusals can terminate before the pure flow runs. |
| Remove one site / profile delete | Rust `delete/profile_check`, `delete/tombstone`, `delete/provider_delete`, `delete/generation`, `delete/queue_remove`, `delete/quarantine_retire`, `delete/complete`; Android boundary is `bridge/profile_delete` / `bridge/retire`; Kotlin provider rows include `profile/capability`, `delete/profile_check`, `delete/provider_delete`, `delete/profile_delete`, `delete/quarantine_retire`. Tombstone is written before provider deletion. `Pending` is `deferred`: generation is secured, but tombstone/ledger remain for startup. In this case `delete/complete/ok` means the protocol returned a deferred action; it does **not** prove the provider profile is gone. The site session and registry are then removed by `commands.rs`, while the durable native profile deletion stays queued. |
| Startup drain / Blocked / Owed | For each queue entry Rust records `delete/profile_delete` with `stepId`; non-Blocked entries get `delete/provider_delete`. `deferred`/`unsupported` remain Owed. After confirmed deletion, Rust checks owner and generation; ambiguous owner/generation is kept or permanently Blocked, never silently lifted. `delete/queue_remove` precedes `delete/quarantine_retire`. `queue/tombstone/blocked` is a permanent name block; it is not the same as a deletion completed in this process. `queue/tombstone/owed` remains queued for retry. |
| Clear-all orphan sweep | Rust `clear/orphan_sweep/started` includes candidate count and duration on completion. Per-profile `clear/profile_delete` carries `stepId`; `clear/tombstone/queued` precedes `clear/provider_delete`. Confirmed deletion/absence is durably `clear/tombstone/blocked` before `clear/quarantine_retire`; deferred/unsupported stays Owed. Existing Blocked names are skipped, not sent to provider again. The launcher's `Default` profile is outside the site-profile namespace. |
| Normal close / flush | Rust `close/start`, `bridge/close`; Kotlin `close/start`, `profile_check`, `resolve`, `flush`, `watchdog_arm`, `late_callback`, `destroy`, `complete`. A normal close destroys the view only after its profile flush succeeds. Clear/delete close with `flush=false` is a different path. `SiteFlushWorker` reports non-sync worker/flush outcomes; routine sync successes are suppressed, while a stuck worker warning is a safe `flush/worker_wait/stuck` duration. |
| Activity destroy | Kotlin `shutdown/destroy`, `shutdown/worker_start`, `shutdown/destroy_flush`, `shutdown/complete`; destroy waits only its 1,000 ms budget and tears down the view even on timeout/failure. A force-stop may bypass `onDestroy`; no in-app row proves a process kill occurred. |
| Android Back | Kotlin `back/back_pressed`; `back_step` IDs 0 fullscreen, 1 WebView history, 2 launcher close; `fullscreen`, `web_history`, `launcher_close`, `poll`, `native_close` and possible `native_fallback`. Poll interval is 200 ms, at most 25 polls. Timeout is pending/unconfirmed—not proof of close. |
| PiP | Kotlin `pip/capability`, `pip_prepare`, `pip_enter`, `pip_playback`, `pip_restore`, `pip_transfer`, `transfer_to_launcher`; Android bridge `bridge/pip_prepare`, `bridge/pip_enter` and `bridge/capability`. Attach/focus transfer failures are safe `provider`/`activity_gone` classes. A false `requestFocus()` and thrown focus callback now fail the transfer result without failing the site's close. |

### Rust-only vs Kotlin-only coverage

| Rust-owned rows | Kotlin-owned rows |
| --- | --- |
| Operation-scope `start` markers; DNS classification; stored generation and queued-name resolution; migration flag persistence; clear protocol's encrypted-session deletion; tombstone/provider-delete/generation/queue-removal/quarantine-retirement ordering; startup drain summary/retry status; orphan-sweep summary; Android plugin call boundary under `bridge/*`. | Android WebView multi-profile capability; profile and view create/bind/attach/navigation lifecycle; provider cookie callbacks; geolocation enumeration/clear counts; Kotlin watchdog/late-callback/quarantine state; flush worker; Android Back ladder; PiP prepare/enter/restore/ownership transfer; app/IPC startup. |

`open`, `migration`, `clear`, `delete`, `close`, and `pip` can therefore have
meaningful rows from both sources, often with the same opaque operation ID.
Rust does not see provider-internal callback detail; Kotlin does not decide the
durable Rust generation/queue outcome. The source field, `bridge` area, phase,
result and operation ID are the intended way to read the cross-layer trace.

**PiP coverage gap:** `PipTransferFlowTest` injects launcher attach/focus actions,
including a false focus result and a thrown focus callback. It does not directly
exercise Android's `enterPictureInPictureMode()` refusal or the platform-driven
page-restore callback; `AnimeHubPipController` remains Android-bound. D-05 is
the real-device check for those provider paths. Do not report a simulated native
PiP refusal as tested by the current unit suite.

### Sensitive logging review and explicit gaps

- Structured events accept only closed codes and validated `r-N`/`k-N` IDs.
  Rust classifies `AppError`; Android bridge classifies plugin errors; Kotlin
  ignores exception details. The inspected Android lifecycle `Log.*` calls and
  Rust `log::*` calls in these paths use fixed text or aggregate counts, not
  provider exception strings, URLs, cookies, or profile names.
- The event schema contains no cookie/Set-Cookie, token/password/AniList
  secret, Authorization value, page/storage content, full URL/host/origin, raw
  site ID/profile name, or geolocation origin. Geolocation code passes origins
  to the provider but records only the number enumerated.
- This is a source audit of the named paths, not a guarantee about Android
  logcat, Chromium/WebView-provider logs, OS crash/ANR reports, old APKs,
  screenshots, unrelated UI error paths, or data added by future code. Review
  and redact those separately using `docs/android-crash-diagnostics.md`.
- Error classes are categories, not stack traces or proof of the exact provider
  cause. Cross-source ordering is best effort, repeated phases are possible,
  and events are lost on restart or ring overflow (with counts for ring drops).
  This feature is intentionally not a per-request/page trace.

## 3. Test-site and safe-sharing rules

Use **only a disposable emulator profile, throwaway AnimeHub site tiles, and a
test HTTPS endpoint with dummy data**. Do not sign in to a production anime,
AniList, or other account. Never use real cookies/tokens as test values.

**Destructive-action warning:** clearing one site deletes that site's provider
cookies/storage and encrypted session data; removing a site may delete its
profile; clear-all affects every registered test site and may sweep unowned
AnimeHub profiles. These actions can permanently delete data and have no undo.
Verify the entire app install is disposable before proceeding. Do not continue
if production data is present.

**Process-kill warning:** force-stop drops both memory-only rings and may
bypass Activity teardown. Export a safe pre-restart timeline, wait until no
clear/delete is pending, and only then force-stop a disposable test install.
A process kill is not proof that `onDestroy` ran.

**Timeout warning:** do not try to make a real provider hang or wait for a real
cookie-delete timeout on a valuable profile. A timed-out uncancellable cookie
wipe can quarantine a profile in the process. Exercise timeout, late callback,
and watchdog-refusal branches with the injected tests in section C; use only
disposable data for any real provider timeout experiment.

Before sharing any export, screenshot, logcat excerpt, or crash report, inspect
it and remove any value matching:

- `Cookie`, `Set-Cookie`, session token, password, `Authorization`, API token,
  AniList client secret, or any value from AniList settings;
- URL, hostname, origin, query string, geolocation origin, page text, storage
  content, raw site ID, profile name, local file path, account name, email, or
  device serial.

The in-app export is allowlisted; if a banned value appears, do not share it.
Share only relevant safe rows (`sequence`, `source`, `area`, `phase`, `result`,
opaque `operationId`, `stepId`, `durationMs`, `watchdog`, `errorClass`,
`count`). A test report should include ID, build SHA, APK build/artifact,
Android API level, WebView provider/version, steps, actual outcome, and whether
it was real provider behavior or injected simulation. Keep pre- and
post-restart exports separate.

## A. Directly runnable on a disposable test emulator

### D-01 — Diagnostics gate, panel, explicit export, and reset

- **Purpose:** Check opt-in collection, same-panel Rust/Kotlin projection,
  safe export, and reset behavior.
- **Prerequisites:** Debug APK from the exact test SHA; disposable app data;
  no site required.
- **Steps:**
  1. Leave developer options off; open Settings and confirm the panel is
     hidden/empty. Enable developer options and refresh the panel.
  2. Perform one harmless launcher action; refresh. Use **Copy** or **JSON
     indir** only if you need an artifact; inspect it with the sharing checklist.
  3. Press **Sıfırla** and confirm old rows are gone and only a safe reset/new
     event remains. Disable developer options and confirm the panel clears.
- **Expected behavior:** Nothing is collected/read while disabled; enabled
  rows appear in the same timeline; copy/download occurs only on button press;
  reset and disable clear prior rows. The app does not send logs externally.
- **Expected log events:** `devlog/gate/enabled`, one or more app/IPC/launcher
  rows, user-triggered `devlog/reset/reset`; panel summary shows `events / 4000`
  and dropped count. Initial startup rows may predate enabling.
- **Failure signal / safe excerpt:** Panel exposed while off, prior rows remain
  after reset/disable, export occurs without request, unknown fields survive
  sanitization, or any banned value appears. Share only the relevant sanitized
  JSON rows and test ID.
- **Classification:** Real app gate/UI/export; no provider behavior injected.

### D-02 — Two site IDs do not share a test cookie

- **Purpose:** Verify real Android per-site profiles using the same origin.
- **Prerequisites:** Two disposable tiles with different site IDs, both pointed
  to a controlled test HTTPS endpoint that can set/check a dummy cookie and
  reports only yes/no. Prefer fresh app data with no legacy jar.
- **Steps:**
  1. Enable diagnostics. Open A and set the dummy cookie; confirm only the
     endpoint's yes/no state. Never copy the value.
  2. Close A normally and wait for close/flush to finish. Open B at the same
     endpoint; confirm A's cookie is absent.
  3. Set B's separate dummy cookie, close B, reopen A and confirm A retained
     only its own session.
- **Expected behavior:** Site IDs, not URL/origin, select isolated profiles.
  On first install, migration may be empty or skipped; the migration flag is
  durable before view creation.
- **Expected log events:** Rust `open/start`, `open/profile_check`,
  `open/resolve`, `migration/migration` (started/skipped and count), optional
  `migration/flag_write`, Kotlin `open/start`, `profile/capability`,
  `profile/profile_check`, `open/view_create`, `open/profile_bind`,
  `open/view_attach`, `open/slot_claim`, `open/navigation_start`,
  `open/complete`; bridge calls under `bridge/migration` / `bridge/open`.
  Empty import can show `migration/complete/empty`. Close should show
  `close/flush`, `close/destroy`, and terminal completion. Same opaque
  operation ID should connect phases for each site open/close.
- **Failure signal / safe excerpt:** B sees A's cookie, A loses its cookie,
  view creation precedes flag write when migration is pending, or a profile
  capability refusal opens via the default profile. Share safe rows and only
  endpoint yes/no outcomes—never the site URL or cookie.
- **Classification:** Real provider/WebView profile behavior with dummy test
  data; not an injected failure.

### D-03 — Clear A without affecting B or launcher state

- **Purpose:** Verify one-site in-place clear, encrypted session handling, and
  launcher/site isolation.
- **Prerequisites:** Disposable A/B tiles from D-02 with dummy data and a
  harmless launcher sentinel (for example a test theme or retained test tile).
  No production data.
- **Steps — destructive:**
  1. Record the launcher sentinel and yes/no cookie status for A and B. Export
     a safe pre-clear timeline if useful.
  2. Use the UI to clear **only A's** site data. Wait for the action to finish;
     do not press Back, kill the process, or start another operation mid-clear.
  3. Reopen A and verify its dummy cookie is gone. Reopen B and verify its
     dummy cookie remains. Confirm the launcher sentinel/settings/tile remain.
- **Expected behavior:** Successful clear removes only A's site-profile data
  and encrypted site session; a failure remains retryable and does not falsely
  delete the encrypted session. No code path falls back to or clears the
  launcher's `Default` profile. The UI sentinel is not a separate cookie-jar
  probe: this APK has no test-only launcher-cookie seeding harness, so do not
  claim this manual test proved a launcher cookie value survived.
- **Expected log events:** Rust `clear/start`, `clear/profile_check`,
  `clear/cookie_wipe`, `clear/cookie_write`, `clear/complete`; Kotlin
  `profile/capability`, `clear/start`, `clear/profile_check`,
  `clear/cookie_wipe`, `clear/geolocation_origins` (count only),
  `clear/geolocation_wipe`, `clear/storage`, `clear/flush`,
  `clear/verified/verified`, `clear/complete`; boundary `bridge/clear`.
  Watchdog/late/error fields appear only if those branches occur. There is no
  `cookie_verify` row. Kotlin flow owns normal clear start/complete and
  geolocation-wipe timing; do not expect a duplicate controller row.
- **Failure signal / safe excerpt:** A still has its cookie after reported
  success, B loses its cookie, launcher sentinel changes, success has no
  `clear/verified`, encrypted session is deleted after a failed wipe, or a
  profile-name/default-profile value appears. Share the clear operation's safe
  rows and yes/no checks only.
- **Classification:** Real provider clear on disposable data. Nonempty
  geolocation permissions and failure/late callbacks are injected in C-02.

### D-04 — Back ladder and normal-close flush

- **Purpose:** Exercise fullscreen → site history → launcher close and normal
  close durability.
- **Prerequisites:** Test page with two controlled history entries; optional
  test video for fullscreen. No production account.
- **Steps:**
  1. Navigate to a second test page and press Android Back once; verify it
     returns through site history rather than closing.
  2. At the first page, press Back; verify the launcher appears. If testing
     fullscreen, enter it first and verify the first Back exits fullscreen.
  3. Wait for close/flush to settle before reopening the test tile.
- **Expected behavior:** One Back press handles only its ladder step. Normal
  close flushes the site profile before destroying its view. A failed or
  timed-out flush keeps the view/session retryable. Poll timeout is pending,
  not proof of a successful close.
- **Expected log events:** `back/back_pressed`; `back_step` IDs 0/1/2;
  `fullscreen` and `web_history` handled/passed; `launcher_close`, `poll`,
  optional `native_fallback`/`native_close`; correlated `close/start`,
  `close/resolve`, `close/flush`, `close/destroy`, `close/complete`, and
  `flush/worker_wait` / `flush/flush` when applicable. A poll `timeout` must
  not be mistaken for a terminal `closed` result.
- **Failure signal / safe excerpt:** Back skips history/fullscreen, a failed
  close destroys the view, or close says success before flush. Share one safe
  operation ID and the first diverging `back_step`/close rows.
- **Classification:** Real Back dispatcher, launcher handshake and provider
  flush. Hanging provider/watchdog branches are simulated in C-04.

### D-05 — PiP prepare, enter, restore, and transfer

- **Purpose:** Verify controlled player-only PiP, restore, playback, and
  launcher/site ownership transfer.
- **Prerequisites:** Emulator API 26+ with PiP hardware feature; controlled
  test video; PiP preference enabled. No production streaming account.
- **Steps:**
  1. Start the controlled video; request PiP using the supported system
     Home/leave-app path. Confirm player-only display.
  2. Toggle play/pause if available, return to the app, and confirm the page is
     restored.
  3. Close the site normally and confirm the PiP target reattaches to the
     launcher WebView and focus is requested.
  4. Optional: repeat with a test page without a video; PiP should refuse
     cleanly and restore the page.
- **Expected behavior:** Preparation precedes native entry; failed preparation
  restores the original page; no stripped page is left behind. Failure to
  attach/focus the launcher is diagnostic and must not turn a safe close into
  a site-data operation.
- **Expected log events:** `pip/pip_prepare`, `pip/pip_enter`,
  `pip/pip_playback`, `pip/pip_restore`, `pip/pip_transfer`,
  `pip/transfer_to_launcher`, `pip/capability`; bridge `bridge/pip_prepare`,
  `bridge/pip_enter`. Unsupported/no-player/request-in-flight and native entry
  failures have safe results; no page text/URL is recorded.
- **Failure signal / safe excerpt:** Whole page appears in PiP, page remains
  altered, playback breaks, or transfer fails unexpectedly. Share safe PiP
  rows, operation ID, API/provider version only.
- **Classification:** Real Android PiP/provider behavior. Attach/focus transfer
  failures are injected in C-05; native entry refusal and platform-driven page
  restore are covered only by this real-device path.

## B. Restart and profile-data preparation

### R-01 — Restart with an empty deletion queue

- **Purpose:** Verify the persisted developer setting and a fresh
  process-local ring; check the empty startup-drain path.
- **Prerequisites:** Disposable install, developer options enabled, no active
  site operation, no pending profile deletion. Export the pre-restart timeline.
- **Steps — process kill:**
  1. Wait for all work to finish and export a safe pre-restart snapshot.
  2. **Warning: force-stop destroys the process-local timeline and may bypass
     `onDestroy`.** On the disposable emulator only, use Android Settings or
     `adb shell am force-stop dev.animehub.app` (verify the package ID for the
     installed build first).
  3. Relaunch, allow startup work to run, inspect the panel, and save a
     separate post-restart export.
- **Expected behavior:** Developer option persists; ring is fresh; no old
  events load from disk. Empty queue is reported. Startup drain is asynchronous
  and is **not** a strict global barrier; per-site open independently resolves
  queued names before migration/view creation under the Android session lock.
- **Expected log events:** `startup/start`, Kotlin `app/start`,
  `startup/capability` if the native bridge applies the gate,
  `queue/profile_list` count 0, `queue/resolve/skipped`,
  `startup/drain/empty`, and `startup/worker_wait/ok`. Async setup events can
  interleave; do not demand a fixed order across sources.
- **Failure signal / safe excerpt:** Old rows survive, queue is unexpectedly
  nonempty, site opens using a queued generation, or a force-stop is described
  as evidence that `onDestroy` ran. Share post-restart rows and pre-restart
  export separately.
- **Classification:** Real app restart/startup; force-stop is an intentional
  destructive test action on disposable app data.

### R-02 — Legacy cookie migration on a disposable upgrade

- **Purpose:** Exercise real nonempty one-time cookie migration and prove the
  flag is durable before the site view is created.
- **Prerequisites:** A separate disposable emulator; a pre-profile build with
  the old encrypted cookie-journal path (for example a locally built 0.3.4
  test APK) and the new test APK signed with a compatible debug key. Use one
  throwaway site and only a dummy cookie from a test endpoint. Never use a
  production account or production app data.
- **Steps — profile-data preparation:**
  1. Install the pre-profile test APK, create the throwaway test tile, set a
     dummy cookie on the controlled endpoint, and allow its normal session sync
     to complete. Close the app normally.
  2. Upgrade **in place** to the new debug APK so encrypted app data remains.
     If signatures/package IDs prevent a safe in-place upgrade, stop and use
     the injected migration tests instead—do not uninstall/reinstall and claim
     the old jar was migrated.
  3. Enable developer options before opening the site. Open the test tile and
     check the endpoint's yes/no cookie status, never the value.
- **Expected behavior:** A virgin profile is created; each dummy cookie is
  accepted and flushed; Rust stores the migration flag before constructing the
  site WebView; the test site retains only its expected dummy state. No
  localStorage/IndexedDB/page data is imported.
- **Expected log events:** Rust `migration/migration/started` with count,
  Kotlin `migration/start`, `migration/profile_check/absent`,
  `migration/profile_create`, per-step `migration/cookie_write/ok` with count
  and step ID, `migration/flush/ok`, `migration/complete/ok`, Rust
  `migration/flag_write/ok`, then Rust `open/open` and Kotlin
  `open/view_create`/`profile_bind`. `view_create` must follow flag-write
  success. No values are in the rows.
- **Failure signal / safe excerpt:** Migration is skipped despite prepared
  legacy test data; view is created before flag-write success; dummy cookie is
  lost after successful migration; a real cookie value or URL appears. Share
  safe migration/open phases and yes/no only.
- **Classification:** Real migration/provider behavior on intentionally
  prepared throwaway legacy app data. A failed/partial real import is not
  required; those branches are injected in C-03.

### R-03 — Deferred delete followed by startup drain (only if provider defers)

- **Purpose:** Observe durable tombstone, generation security and next-process
  drain when the provider actually reports an in-use/deferred profile.
- **Prerequisites:** Disposable test site/profile; no launcher or other site
  data worth keeping. Provider behavior may not naturally return `Pending`.
- **Steps — destructive profile deletion:**
  1. Export current safe rows. Remove the throwaway test tile only after
     confirming the install contains no production data; this may permanently
     delete its encrypted session/profile.
  2. Continue only if rows show `delete/tombstone/queued` before
     `delete/provider_delete/deferred` and `delete/generation/ok`. If provider
     deletion is `deleted`/`absent`, this did not create a drain case—stop.
  3. Wait for the command to finish; export the pre-restart log. With no
     operation pending, force-stop/relaunch as in R-01 and save a separate
     post-restart log.
- **Expected behavior:** A deferred delete is never reported as provider
  deletion complete and its tombstone/ledger stay owed. Startup retries; a
  confirmed provider result is followed by safe owner/generation proof,
  durable queue removal and quarantine retirement. Blocked/unmatched records
  are retained, not lifted. `delete/complete/ok` before restart can mean only
  the command recorded a deferred result.
- **Expected log events:** Pre-restart Rust `delete/tombstone/queued`,
  `delete/provider_delete/deferred`, `delete/generation/ok`,
  `bridge/profile_delete/deferred`, terminal `delete/complete`; no
  `queue_remove` for the pending provider result. Post-restart
  `startup/drain/started`, queue counts, per-entry `delete/provider_delete`,
  `delete/generation`, `delete/queue_remove`, `delete/quarantine_retire`, and
  terminal `startup/drain/ok|unavailable|failed` / worker outcome.
- **Failure signal / safe excerpt:** Provider is called before a durable
  tombstone, old generation can be reopened, unmatched owner is lifted, or an
  Owed entry disappears without confirmation. Share only safe pre/post rows;
  do not include the site ID/profile name.
- **Classification:** Real provider behavior only if it naturally returns
  deferred. If not, mark this test **not reproduced** and use C-04 fakes; do
  not repeatedly delete/recreate data to force `Pending`.

### R-04 — Clear-all orphan sweep (highest data-loss risk)

- **Purpose:** Exercise real provider profile enumeration and clear-all orphan
  handling, if a disposable install has an AnimeHub-owned orphan.
- **Prerequisites:** Disposable install with only throwaway tiles/data and an
  existing orphan candidate from a test flow. A normal install may have no
  orphan. Do not fabricate profile names or edit provider storage manually.
- **Steps — destructive, permanent for all test sites:**
  1. Confirm every registered site is disposable; export safe rows. Remember
     clear-all removes all registered test-site cookies/storage and encrypted
     sessions, not just the orphan.
  2. Run **clear all** once and wait for it to return; do not interrupt it.
  3. Verify each registered test site's dummy cookie is gone and launcher
     settings/tile sentinel is unchanged. Inspect only counts and safe
     per-operation rows.
- **Expected behavior:** Registered site profiles are cleared first. Orphans
  are limited to the AnimeHub site-profile namespace. Each deletion is
  tombstoned before provider work; confirmed deletion is permanently Blocked
  before quarantine retirement; deferred/unsupported remains Owed; existing
  Blocked rows are skipped. Launcher `Default` is excluded.
- **Expected log events:** Rust `clear/orphan_sweep/started` with count and
  terminal `ok|failed`; each `clear/profile_delete` has `stepId`, then
  `clear/tombstone/queued`, `clear/provider_delete`, and, for confirmed
  deletion, `clear/tombstone/blocked` → `clear/quarantine_retire/retired` →
  `clear/profile_delete/deleted`. Deferred/unsupported has no Blocked/retired
  completion.
- **Failure signal / safe excerpt:** Any non-test profile appears affected,
  provider delete precedes tombstone, a Blocked name is deleted again, an Owed
  record is dropped, or the sweep reports success after a hard failure. Share
  safe count/phase rows only. Do not run this when production data is present.
- **Classification:** Real provider enumeration/deletion only if an orphan is
  already available. The exact Blocked/Owed/owner failure permutations are
  deterministic fakes in C-04; otherwise mark this real case skipped.

## C. Injected-failure tests (simulations, never real provider outcomes)

No shipping debug command injects failures. These tests drive the production
sequencing/decision code through fakes; they do **not** write a real WebView
profile or prove Android provider timing. Run them before destructive emulator
steps. A fake timeout/refusal must be reported as simulation, not as provider
behavior.

### C-01 — Diagnostics schema, gate, merge, overflow, redaction and export

- **Purpose:** Test the shared panel projection, opt-in gate, mixed-source
  ordering/correlation, bounded ring/drop count, unsafe-field stripping and
  user-triggered clipboard/download boundary.
- **Prerequisites:** Node/npm from repository root. For Android/Rust source
  checks use their test suites/CI; local Rust/Android toolchains may be absent.
- **Steps:** Run `node --test tests/debug_log.test.js tests/copy_text.test.js`
  and `node --test tests/android_isolation.test.js`. In Android CI run
  `AnimeHubDebugLogTest`; in Rust CI run the `diagnostics` tests.
- **Expected behavior:** Both rings stay gated; each caps at 2,000; combined
  snapshot is capped at 4,000 and reports drop count; rows sort by
  timestamp/source sequence and retain a safe operation ID; redaction drops
  unknown values/fields; explicit reset and copy fallback remain safe.
- **Expected log events:** Test-created rows for Rust/Kotlin merge and the safe
  `devlog/gate` / `devlog/reset` markers. Unit tests assert the snapshot rather
  than writing a diagnostic file.
- **Failure signal / safe excerpt:** Failing test name/assertion; any raw
  fixture value surviving sanitization; event while disabled; capacity or
  dropped count mismatch. Share the assertion and a fixture-free excerpt only.
- **Classification:** Injected unit-test snapshots/fakes; not device/provider
  behavior.

### C-02 — Clear phases, provider failure, watchdog, late callback, quarantine

- **Purpose:** Verify every clear step, late behavior, safe error classes, and
  unchanged clear/quarantine/session safety with diagnostics enabled.
- **Prerequisites:** Android Gradle unit-test source set generated by
  `python3 scripts/android_prepare.py`; run the Kotlin test source set copied
  by CI. Do not induce real timeouts.
- **Steps:** Run `SiteProfileOpsTest` cases including `clear success runs every
  step in order`, `diagnostics are observational and redact a clear failure
  without changing its safety outcome`, and `diagnostics record a timed-out
  cookie wipe and late settlement without reopening the profile`. Also run the
  cookie/geolocation/storage/final-flush failure and late-callback cases,
  `a refused watchdog fails the step and never starts the provider call`, and
  the per-operation quarantine retry tests.
- **Expected behavior:** Settled failure remains failure while later required
  steps run; encrypted session retention is unchanged on failure; an issued
  uncancellable cookie delete that times out quarantines its own operation;
  late callback settles only that record; no open until all earlier operations
  settle and a later wipe verifies. Refused watchdog starts no provider work.
- **Expected log events:** `clear/cookie_wipe/failed` with `provider`, or
  `timeout` + `watchdog=fired`; `profile/quarantine/quarantined`; later
  `clear/late_callback/late` with `watchdog=late`; geolocation/storage/flush
  phase results; refusal with `watchdog=refused` and
  `errorClass=watchdog_refused`; terminal `clear/verified` only after a
  verified success. The existing diagnostic safety test uses an opaque test ID
  and asserts the secret fixture is absent.
- **Failure signal / safe excerpt:** False success, lost quarantine ownership,
  late mutation/result changing another operation, encrypted session dropped
  after failed wipe, or fixture text in the snapshot. Share test name/assertion
  and safe event rows only.
- **Classification:** Injected Kotlin fake provider/watchdog outcomes; not a
  real WebView provider result. **Do not create real provider timeouts for this
  test.**

### C-03 — Migration/open, partial import, flag-write failure, quarantine

- **Purpose:** Verify migration-before-view ordering and the one-time retry
  path only for a never-shown partial profile; quarantine is not misread as a
  migration retry.
- **Prerequisites:** Rust host tests and Kotlin Gradle test source set.
- **Steps:** Run Rust tests `protocol_aborts_before_the_view_when_the_flag_write_fails`,
  `partial_migration_retires_once_retries_and_keeps_the_jar`,
  `second_partial_migration_fails_visibly_without_opening_anything`,
  `quarantine_refusal_never_retires_the_profile_or_writes_the_flag`, and
  Kotlin migration tests `migration requires a virgin profile and never
  touches an existing one`, `migration completes only when every cookie was
  accepted and flushed`, `a rejected cookie fails the migration and stops the
  chain`, `migration watchdog turns a lost cookie callback into a retryable
  failure`, and `a refused migration watchdog writes no cookie and claims no
  success`.
- **Expected behavior:** Each cookie write is counted but never exposed; only
  all-accepted plus durable flush succeeds; Rust flag is stored before
  `open/open`; flag failure leaves view unopened and legacy jar intact. A
  partial import gets tombstone → generation advance → one retry; second
  partial remains retryable. Quarantine refusal never retires a live profile or
  writes the migration flag.
- **Expected log events:** Kotlin `migration/profile_check`,
  `profile_create`, `cookie_write` steps, `flush`, `watchdog_arm`,
  `late_callback`, `complete`; Rust `migration/migration`, fallback
  `migration/tombstone`, `migration/generation`, `migration/flag_write`,
  `open/open`, `open/complete`; `profile/quarantine` on gate refusal. Failure
  branches carry safe `migration_partial`, `storage`, `timeout`, or
  `quarantined` classes; no successful view event after failed flag persistence.
- **Failure signal / safe excerpt:** `view_create` before `flag_write/ok`,
  partial import treated as complete, jar removed after failed open, quarantine
  routed into profile retirement, or raw cookie fixture in output. Share test
  names and safe phase/result rows only.
- **Classification:** Injected Rust storage/plugin and Kotlin callback fakes.
  A simulated rejection/timeout is not a real provider result.

### C-04 — Delete/drain/generation, tombstone, Blocked/Owed and orphan sweep

- **Purpose:** Test destructive ordering and crash-recovery decisions without
  touching an emulator profile.
- **Prerequisites:** Rust host suite with the protocol/storage/provider fakes
  in `src-tauri/src/web/windows.rs`.
- **Steps:** Run the Rust tests separately with
  `cargo test --manifest-path src-tauri/Cargo.toml mobile_delete_protocol`,
  then filters `drain_`, `orphan_sweep`, and `generation_past_name`; also run
  the unmatched-owner/Blocked cases. Finish with the full CI command
  `cargo test --all`.
- **Expected behavior:** Tombstone precedes provider delete; generation is
  monotone/idempotent/overflow-safe; only owner-matched or provably orphaned
  names are made underivable; Blocked is never re-deleted or silently removed;
  Owed/unsupported/unmatched stays durable; queue removal precedes quarantine
  retirement; orphan sweep preserves the same rules.
- **Expected log events:** Rust `delete/tombstone`, `provider_delete`,
  `generation`, `queue_remove`, `quarantine_retire`, `queue/resolve`,
  `queue/tombstone/blocked|owed`, `startup/drain`, and per-orphan
  `clear/orphan_sweep`/`clear/profile_delete` with `stepId`/counts. Rust
  diagnostics tests separately assert merged source ordering, gate, ring/drop
  and redaction.
- **Failure signal / safe excerpt:** Failing kill-point assertion, reused
  generation, mismatched owner lifted, provider call before tombstone, Blocked
  retried, Owed dropped, or profile name in output. Share test name/assertion
  and safe codes only.
- **Classification:** Injected Rust storage/provider fakes; not Android
  provider behavior and not a device test.

### C-05 — Close/destroy, Back fallback, and injected PiP attach/focus failure

- **Purpose:** Exercise safe flush, teardown, Back fallback, and the
  Android-free PiP launcher-transfer decision without killing or wedging a
  real WebView.
- **Prerequisites:** Kotlin Gradle tests and host Rust tests. Keep provider
  failure injection in fakes.
- **Steps:** Run `SiteProfileOpsTest` close flush success/failure/timeout/late
  cases, destroy-time budget/timeout/failure cases, Back status tests, and all
  `PipTransferFlowTest` cases, including `launcher transfer still requests
  focus after attach failure`, `launcher focus refusal is a failed transfer
  even when attach succeeds`, `launcher focus exception is safely classified`,
  and missing-launcher behavior. Run Rust open/close/command tests as available.
- **Expected behavior:** Failed normal close retains view/ownership; destroy
  tears down but labels unverified durability honestly; Back timeout remains
  pending, not closed; PiP attach/focus failure is non-fatal to site close;
  false `requestFocus()` and thrown focus are both transfer failure. This suite
  does **not** exercise native PiP entry refusal or platform-driven restore.
- **Expected log events:** `close/flush`, `close/late_callback`,
  `shutdown/destroy_flush`, `shutdown/complete`, `back/poll`,
  `back/launcher_close`/`native_close`, `pip/pip_transfer`,
  `pip/transfer_to_launcher`; safe `provider`, `timeout`,
  `watchdog_refused`, or `activity_gone` classes. `pip/pip_enter` and
  `pip/pip_restore` for native behavior belong to real emulator test D-05.
- **Failure signal / safe excerpt:** View destroyed after failed normal flush,
  timeout described as durable, pending poll described as closed, duplicate
  close, focus false/throw reported as success, or exception text in snapshot.
  Do not describe an untested native PiP refusal/restore branch as passed.
  Share test name/assertion and safe rows only.
- **Classification:** Injected Kotlin/Rust fakes. Not actual Android Handler,
  WebView, focus, or PiP native API behavior.

## 4. Android CI, APK and verification status

`scripts/android_prepare.py` copies the Kotlin production sources and unit-test
files into the generated Gradle project. The Android CI job runs the Kotlin
policy tests (including `AnimeHubDebugLogTest.kt` and
`PipTransferFlowTest.kt`), compiles the production controller, then builds the
aarch64 debug APK at `dist-apk/animehub-aarch64-debug.apk` and uploads artifact
`animehub-android-aarch64-debug`. The host Rust CI job runs `cargo test --all`;
Node checks are separate. A successful APK build is not an emulator/provider
test, and a pre-change CI run is not verification for a new PR head.

For every handoff, report separately:

1. local Node checks and `git diff --check`;
2. local Rust/Gradle availability and any tests actually run;
3. CI URL/status for the exact new SHA;
4. APK build status and artifact URL (not just “Android CI passed”);
5. real emulator/provider tests completed, skipped, or not reproduced;
6. PR #27 head/base/open state and local worktree state.
