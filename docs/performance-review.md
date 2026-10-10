# Performance review: site open path

Status: **source review only.** No Android device or desktop measurement was made.
No optimisation was applied, because nothing here is measured yet.
Labels: **verified** = read in the source (file and line given); **assumed** = not
measured, or an inference about runtime cost.

## Findings

| # | Where | What happens | Status |
|---|---|---|---|
| P1 | `commands.rs` `open_site` | DNS classification runs in `spawn_blocking`, off the async runtime. | verified |
| P2 | `commands.rs` `open_site` | `validate_site_url` and DNS run on every open, not cached. | verified |
| P3 | `web/session.rs` `build_init_script` | Appends the 33 KB `pip_controller.js` string on every Android open. Cost of the copy: **assumed** negligible next to WebView load. | verified (size), assumed (cost) |
| P4 | `web/windows.rs` open watcher (~592–620) | Polls `window.url()` every 250 ms for up to 15 s, then runs the storage restore. Whether `window.url()` on Android crosses to the UI thread on each poll: **assumed, not verified**. | verified (loop), assumed (cost) |
| P5 | `web/windows.rs` `spawn_session_sync` (desktop ~385, mobile ~419) | One thread per open site, ticking every 30 s. Each thread exits when its session is no longer current. Threads do not pile up across site switches. | verified |
| P6 | `AnimeHubPlugin.kt` `webview_cookies_replace` | Restores cookies one `setCookie` at a time. Each callback starts the next call, so restore latency grows with the number of cookies (one round trip each). Callbacks arrive on the UI thread; the time is not measured. | verified (structure), assumed (cost) |
| P7 | `AnimeHubPlugin.kt` `flushCookieStoreAsync` | Disk flush is already off the UI thread. | verified |
| P8 | `web/dns.rs` via desktop `on_navigation` | `classify_host` does synchronous DNS inside the navigation callback. This can block the UI thread on desktop for each new host. | verified (earlier review) |

## Why nothing was changed

- P6 is the most likely real cost. Sending all `setCookie` calls at once would remove
  the round trips. But Chromium's ordering of those calls is **assumed**, and a change
  to cookie ordering is a security-relevant behaviour change. It needs a measurement
  and a test on a device first.
- P8 is a security control. Caching DNS answers would change the DNS-rebinding guarantee.
  It is not a low-risk optimisation.
- P3 and P4 are small in the source. Caching the init script or replacing the poll with an
  event would be guesses without numbers.

## Measurement plan (device, not yet run)

Existing hook: `Cookie jar ready: N cookie(s), Xms` (logcat tag `AnimeHubPlugin`)
already reports the cookie restore time.

Metrics:
1. `open_site` duration from tap to `OpenedSite` (add a timer in Rust, log via `log::info!`).
2. Cookie restore time, from the existing log line, per cookie count N.
3. Time from `open_site` to the storage restore (watcher exit), i.e. the 250 ms poll latency.
4. Tap-to-first-paint of the site, from the logcat timestamps (or a frame-timing trace).
5. PiP preparation time (`prepare` call to `enter_pip` answer), with an existing debug report.

Procedure:
- Device: one physical Android phone, debug APK from CI (`animehub-android-aarch64-debug`).
- Same site, same session, cold start and warm start, 10 opens each.
- Record the median and the spread. Run once before and once after any change, with
  the same APK except the change.
- Only a change whose median improves beyond the spread counts as an improvement.

Desktop: `open_site` timing on Linux with the release build, same procedure.

## Not verified

- No metric above has been collected. Any improvement claim requires the plan above.
