# Residual security risks after PR #27 fixes

This document records what this branch fixes and what it deliberately does
**not** claim to fix.

## Fixed

### Android OAuth callback interception (R-05)

The Android intent filter for `animehub://anilist/callback` is **removed**
(`plugins.deep-link.mobile` is empty). Native AniList login on Android accepts
only a loopback redirect, the same mechanism desktop uses. A login started with
a custom scheme is refused before any pending state is written. Another app can
still register the scheme, but there is no OAuth intent for it to receive.

### Third-party cookie migration lockout

The legacy 0.3.4 jar was captured from the shared WebView store and contains
CDN/player/ad cookies. `setCookie(site_url, …)` rejects foreign domains. Rust
keeps only host-only cookies and cookies whose Domain is the site host or a
parent of it; Kotlin skips an individually rejected entry. The durable
`flush()` remains the success criterion.

### Stuck flush worker

After `FlushRecovery.REPLACE_AFTER_MS` of measured stall, the next submission
receives a fresh single-thread executor; the abandoned daemon thread and its
queue are discarded. Callers keep their watchdogs/bounded awaits and settle as
failures. This does not make an in-flight flush cancellable, but it prevents a
permanent pile-up.

### Android user blocklist (R-01)

The blocklist used to be enforced on desktop only. `Blocklist::entries()` now
serialises the user's rules; Rust pushes the snapshot to the Android plugin
before every site open and after every settings change; Kotlin evaluates the
same suffix / `*` / `=` semantics in both the top-level navigation checkpoint
and the sub-resource checkpoint. Matching is pinned by Kotlin unit tests,
including the `notdoubleclick.net` lookalike.

### Disabled blocklist on Android

The rule text stays in Rust; the WebView receives a parsed snapshot. A
**disabled** blocklist therefore arrives as an **empty** snapshot, produced by
`android_blocklist_entries` from the same `Blocklist::is_enabled()` gate that
`is_blocked` honours. Previously the rules were pushed while disabled, so
Android kept blocking after the user turned the list off.

### Hidden sites are reversible again

`list_sites` returns the visible grid only, so the restore path needs its own
read: `list_hidden_sites` plus `set_site_hidden(id, false)` from
Ayarlar → Gizlenen siteler. Hiding still keeps the entry, its icon and its
profile; nothing is deleted either way.

### Poisoned locks

`commands::lock_recover` takes the guard out of a poisoned mutex instead of
panicking. The old `lock().expect("… lock")` sites sat in the navigation
callback, so one panic elsewhere (a flush or resolver task holding
`current_site`, `launcher_url` or `blocklist`) turned into a permanent panic
loop. Recovery is not a correctness proof: a panic between two writes can still
leave the in-memory state at the last completed step, which is exactly what the
persisted state is re-read for on startup.

### Android minimum version

`minSdkVersion` is 29 (Android 10), the multi-process WebView baseline for
`MULTI_PROFILE`. Unsupported providers still fail closed with updated guidance.

### Supply chain

The hardened workflows in `ci-fixes/` pin every external Action to a commit
SHA (install with `ci-fixes/apply.sh`; the branch push token had no workflow
scope). A test rejects mutable tags in those files.
`android_prepare.py` fails unless each Gradle pin appears exactly once, and CI
inspects the resolved dependency graph.

## Round 6 — what is fixed and what is still blocked

### Android blocklist is fail-closed

`commands::sync_android_blocklist` returns `AppResult<()>`
(`AppError::BlocklistUnavailable`). It is called by `open_site`,
`update_settings` and `reset_blocklist`. The transfer helper first pushes the
snapshot; only if that fails does it close an already-open site through the
normal lifecycle-locked close path. The command then fails instead of returning
a success view. A failed close is a separate restart-required `Other` error, not
a claim that the old snapshot is harmless — that state stays risky until the
user restarts. Rust tests pin the push→close order and the no-success-on-close-
failure rule; the Android close/device path is still host/CI-only (R-04).

A disabled list still arrives as an **empty** snapshot (Kotlin has no enabled
flag), which is the only representation that matches `Blocklist::is_blocked`.
A transport failure is never converted into that empty snapshot.

### Blocklist transfers are ordered, so a stale snapshot cannot win

Fail-closed answers "what if the push fails". It did not answer "what if two
pushes race", and they can: `sync_android_blocklist` runs from `open_site`,
`update_settings` and `reset_blocklist` **outside** `ANDROID_SESSION_LOCK`,
because that lock guards the site lifecycle and the fail-closed close path has
to take it. Kotlin's `SiteBlocklist.update` is a last-writer-wins atomic
reference swap, so a transfer built from older settings could complete *after*
a newer one and silently restore the old rules — the persisted settings, the
Rust matcher and the Kotlin snapshot would then disagree with no error on any
side.

`transfer_blocklist_with_generation` closes that with two mechanisms:

* `BlocklistSyncGate::lock` serializes transfers, so at most one push is in
  flight. Lock order is `BlocklistSyncGate::lock` → `ANDROID_SESSION_LOCK`;
  nothing acquires them the other way round and the close path never takes the
  gate lock, so the existing lifecycle rules still hold and this cannot
  deadlock against them.
* The snapshot is **re-read inside that lock** (`blocklist_transfer_snapshot`)
  and compared against `applied`. A caller whose generation is not newer returns
  `Ok(())` *without pushing* — and, because that is the success path, without
  touching a live session. `rebuild_blocklist` bumps the generation while the
  matcher lock is still held, so new rules can never be observed under an old
  generation. `applied` advances only after a confirmed push, so a failed
  transfer is retried instead of being believed.

`BlocklistSyncGate::new()` starts `generation = 1` and `applied = 0` on purpose:
`lib.rs` builds the startup `Blocklist` directly rather than through
`rebuild_blocklist`, and Kotlin starts empty, so treating "nothing delivered
yet" as 0 on both sides would have let the first `open_site` skip the push and
run the site WebView with no user rules at all.

Coverage: the nine `*_blocklist_*` / `*_transfer_*` tests in `commands.rs`
drive the real guarded function against a fake Kotlin side, including a
multi-threaded out-of-order case asserting Kotlin's delivered generations are
monotonic and end on the newest rules. **These are host tests.** They were
written in an environment with no Rust toolchain, so they have not been executed
here — `cargo test` in CI is the first run (R-04).

### Redirect DNS verdicts (partial)

`NavigationDnsGate` is shared by `shouldOverrideUrlLoading` and
`shouldInterceptRequest`. The top-level checkpoint cancels a target whose
**cached** answer is private or changed. On a cache miss it returns
`ALLOW_AND_PROBE`: the navigation proceeds before the DNS answer for that target
exists, and the answer is only cached for later requests. This is forced by the
synchronous WebView callback (no UI-thread resolver), but it is not a guarantee
for the navigation that triggered the probe. It is also **not** connection
pinning: the engine resolves again when it connects, so a rebinding host can
answer public to the check and private to the socket. A network layer that pins
the connected address would be needed; the options that were actually evaluated
(`androidx.webkit.ProxyController` / `PROXY_OVERRIDE` and the rejected
alternatives) are written up under
[DNS rebinding on Android](#dns-rebinding-on-android), together with the device
matrix that has to run before any of it can be shipped. That network layer is
**not** part of this PR. **R-01 (b) stays open.** No comment, test or doc change
closes it.

### OAuth loopback redirect must be exactly the served endpoint

`loopback_port()` now accepts only `http://127.0.0.1:<explicit port>/callback`
— no `/cb`, no omitted port (80), no query/fragment/userinfo, no `localhost`
or `[::1]` (the listener binds `127.0.0.1`). `validate_redirect_uri()` runs
**before** pending state is written and the listener is started only for
`LoginRedirect::Loopback(port)`, so an unusable URI can no longer burn a state
value and hang the login.

### Flush worker replacement is bounded

`shutdownNow()` cannot stop a native provider call blocked in the platform, so
each abandoned worker keeps a thread. `FlushRecovery.MAX_ABANDONED_WORKERS = 2`
bounds replacements; after that the worker **refuses** new submissions
(`execute` returns `false`, `submit` settles the caller with an error) instead
of creating more threads. Real provider latency and kill paths remain device
work (R-04).

### Actions pinning — explicit blocker

The hardened, **SHA-pinned** workflows live only in `ci-fixes/*.yml`; the
**active** `.github/workflows/*.yml` still reference mutable tags. The push of
this round's active-workflow pinning was rejected with:

```
refusing to allow a Personal Access Token to create or update workflow
`.github/workflows/ci.yml` without `workflow` scope
```

The permission was not worked around, nothing was pushed to `main`, and no tag
or release was created. The item stays **open** until a token with `workflow`
scope (or a maintainer running `ci-fixes/apply.sh`) lands the pinned files.

## Partially mitigated / residual

### DNS rebinding on Android

`shouldInterceptRequest` resolves hostnames and refuses:

* any non-empty answer containing a private, loopback, link-local, reserved or
  malformed address;
* a public answer that **differs** from the first pinned public set for that
  host (flip detection);
* all IP literals and built-in host suffixes.

Residual: **TOCTOU**. The WebView engine resolves again when it connects. A
rebinding server could answer a public address to both pre-checks and a private
one to the engine. The complete fix is a network layer that pins the resolved
address for the connection, not another WebView callback. A resolver outage is
deliberately fail-open so an offline DNS server does not brick the app.

#### Network-layer options actually evaluated (and why none is implemented here)

This was researched against the platform APIs rather than assumed. Findings:

* **`androidx.webkit.ProxyController` / `ProxyConfig` — the only supported
  WebView network hook.** Feature constant is `WebViewFeature.PROXY_OVERRIDE`
  (added in androidx.webkit 1.1.0); `getInstance()` throws
  `UnsupportedOperationException` unless `isFeatureSupported` is true. A loopback
  **CONNECT-only** proxy installed through it would resolve the hostname itself
  and open the socket only to an address it has already classified, so the
  resolver used for the check *is* the resolver used for the connection — that
  is what actually closes the TOCTOU window. TLS stays end-to-end: CONNECT
  forwards bytes unread, so certificate validation and the system-CA-only trust
  policy are untouched, and each redirect hop is a new request through the same
  guard. `androidx.webkit` is already a dependency here (for `MULTI_PROFILE`).
* **Why it is not implemented on this branch.** The API is process-wide
  ("used by all WebViews in the app") and asynchronous — "network connections
  are not guaranteed to immediately use the new proxy setting; **wait for the
  listener before loading a page**" — so it has to be ordered against
  `open_site` and the activity lifecycle. `addDirect()` must be *omitted*,
  otherwise the engine falls back to a direct connection that bypasses the
  guard; omitting it means a proxy failure is a total loss of connectivity, so
  the failure path needs its own design. The loopback listener is reachable by
  other apps on the device, so it must not become a usable tunnel for them.
  HTTP/3/QUIC is not carried by an HTTP CONNECT proxy and needs to be confirmed
  disabled under a proxy override. The launcher WebView (local content) needs a
  bypass rule. **None of that can be verified in this environment: there is no
  Android SDK, no Gradle, no JVM, no device and no emulator here**, so TLS
  behaviour, redirects, QUIC, provider differences and lifecycle ordering would
  all be unverified. Shipping it unverified would trade a documented residual
  risk for an undocumented one — and a bug in a mandatory proxy breaks every
  site, not just the rebinding case.
* **Rejected alternatives.** `shouldInterceptRequest`-side fetching would mean
  reimplementing the network stack (streaming video, range requests,
  `CookieManager` sync, HTTP auth, service workers) and is not called for every
  main-frame navigation. Chromium's `--host-resolver-rules` is not settable on
  Android WebView. `ConnectivityManager.bindProcessToNetwork` selects a network,
  it does not filter destinations. A `VpnService` filter needs user consent for
  a VPN and is a different product decision. Tauri's Rust HTTP client never
  carries WebView traffic, so nothing in `src-tauri/src/web/dns.rs` can pin a
  WebView connection.
* **Decision left to a human.** Implementing the `PROXY_OVERRIDE` guard is a
  real, bounded project — but it needs a device/emulator matrix first:
  (1) `isFeatureSupported(PROXY_OVERRIDE)` across provider versions including
  the downgrade path; (2) proof that no request escapes when `addDirect()` is
  absent; (3) TLS to a public host through the tunnel, including redirects and
  a rebinding host that flips its answer mid-chain; (4) private/loopback/
  link-local destinations refused at CONNECT; (5) resolver failure behaviour;
  (6) HTTP/3 confirmed off; (7) the launcher WebView still loads; (8) another
  app on the device cannot use the loopback listener as a tunnel; (9) activity
  recreation / process restart ordering. Until that runs, `ALLOW_AND_PROBE`
  stays as it is and **R-01 (b) stays open**.


### Device verification (R-04)

No physical device or emulator was available. Remaining release gates:

* MULTI_PROFILE present/absent on two provider versions;
* cookie/localStorage separation between two sites;
* default-profile data untouched;
* one-time migration from a real 0.3.4 jar;
* process kill between tombstone, provider delete and generation write;
* late `removeAllCookies` / flush callbacks and worker recovery;
* PiP, fullscreen, back and Android system-back handoff;
* blocklist push observed inside the real WebView.

Host and Kotlin tests use fake ports; they are not a substitute for the matrix.

### PR structure

`windows.rs` and the delete/drain/sweep protocols remain large. Splitting them
without a Rust test run in this environment would be an unverified refactor; it
is tracked as the next stacked PR.
