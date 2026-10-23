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
SHA, and a test rejects mutable tags in those files. The **active** workflows
still use mutable tags: the branch token has no `workflow` permission and the
push was refused again this round — see
[Actions pinning — explicit blocker](#actions-pinning--explicit-blocker) for the
verified SHA table and the exact change a maintainer has to make.
`android_prepare.py` fails unless each Gradle pin appears exactly once. The
`ci-fixes/` proposal to also assert the **resolved** Gradle graph in CI is
deliberately unapplied: it cannot be exercised in this environment.

## Round 16 — DNS rebinding is decided where the connection is made

### The guarantee, stated precisely

For an Android **site WebView**, the IP address actually connected to is now
the IP address that was classified. That is not a check that runs earlier or
more often; it is a different component making the decision:

```
WebView ──CONNECT host:port──▶ DnsPinnedProxy (127.0.0.1:ephemeral)
                                    │ 1. parse the authority, refuse anything ambiguous
                                    │ 2. resolve the name ONCE
                                    │ 3. classify EVERY address with IpPolicy
                                    │    (a mixed answer is refused as a whole)
                                    ▼
                              PinnedConnect.connect(InetAddress, port)
                                    │ InetSocketAddress(address, port) — no lookup
                                    │ then verify socket.inetAddress == address
                                    ▼
                              byte tunnel (TLS untouched, end-to-end)
```

Because the resolver call that produced the vetted address *is* the resolver
call that produced the socket argument, a name cannot answer one address to the
check and another to the connection: there is no second query to poison. That
is the entire fix, and it is why the `ALLOW_AND_PROBE` behaviour of the old
pre-check no longer matters for a site view — the pre-check is no longer what
decides the connection.

Files: `DnsPinnedProxy.kt` (proxy, CONNECT parsing, classification, bounded
tunnel), `PinnedConnect.kt` (the only place a socket is opened, plus the
verification), `SiteProxyGate.kt` (install state, config policy, refusal),
`SiteProxyController.kt` (the `ProxyController` adapter and the open gate).

### Fail-closed, in one table

| Situation | Behaviour |
| --- | --- |
| `PROXY_OVERRIDE` not supported by the provider | site open refused (`proxy-unavailable`) |
| loopback proxy cannot bind | site open refused |
| `setProxyOverride` throws | site open refused |
| install callback never arrives (5 s) | site open refused |
| proxy stopped/unreachable | WebView has **no** route: never a direct fallback |
| DNS answer empty, throws, or is mixed public+private | CONNECT refused (502/403), socket closed |
| any loopback/private/link-local/reserved/ULA/mapped/reserved-literal address | CONNECT refused (403) |
| cleartext HTTP through the proxy | refused (405) — the policy is HTTPS/WSS only |
| connection ceiling reached | refused (503), no queueing |
| socket's remote address differs from the vetted one | tunnel refused, socket closed |

`ProxyConfig` is built with exactly one rule and nothing else:

```kotlin
ProxyConfig.Builder().addProxyRule("127.0.0.1:<ephemeral>").removeImplicitRules().build()
```

No `addBypassRule`, no `addDirect()`, no `bypassSimpleHostnames()` — each of
those is a documented way to make a request skip the proxy — and the implicit
localhost/link-local bypass is removed explicitly. A fixed (non-PAC) proxy list
containing no `DIRECT` entry has no fallback: a failed proxy is a failed
request, which is the property the whole feature depends on. `clearProxyOverride`
is never called, not on close, not on a failed open, not on Activity destroy —
clearing it means "all connections direct".

### Why the WebView is not exempted from its own proxy

`ProxyController` is process-wide, so the Tauri launcher is governed too. Its
own origin is served *before* any network I/O: wry's `RustWebViewClient`
answers `WebViewAssetLoader`'s domain (and the Tauri IPC path) inside
`shouldInterceptRequest` (verified in wry 0.55.1's `RustWebViewClient.kt`), so
those requests never reach a proxy. This is deliberate: no bypass rule is
added, because a bypass would also let a *site* page reach a `.localhost`
address directly. If a future Tauri version moved the launcher to a real
loopback HTTP server, the proxy would classify it, refuse it as loopback, and
the launcher would break — loudly and closed, which is the direction a security
regression has to fail in. The device checklist includes this check.

The site WebView's own navigation policy still refuses `tauri.localhost`,
`.localhost`, `.local`, `.internal`, `.lan`, `.home` and IP literals at both
checkpoints, so the app's IPC origin stays unreachable from a page even if the
proxy were misconfigured.

### What is still NOT claimed

* **No device verification yet.** There is no Android SDK, emulator or physical
  device in this environment: the proxy, the pinned connect, the CONNECT parser
  and the tunnel are covered by real-socket JVM tests and the whole chain is
  compiled by the Android CI job, but the WebView↔proxy path itself (does the
  engine send `CONNECT` for every HTTPS/WSS request, does it disable QUIC under
  a proxy, does the launcher still render) is unverified. See
  `docs/android-dns-pinning-device-checklist.md`; until that matrix runs, this
  is "closed by design and by unit/socket tests", not "observed on a device".
* **Desktop is unchanged.** Tauri's desktop child WebView has no equivalent
  network hook; `web/dns.rs` + `decide_navigation` remain a pre-check with the
  same TOCTOU shape. Nothing in this round changed desktop behaviour or its
  documentation.
* **The launcher WebView before the first site open.** The override is
  installed on the first site open (and kept for the process lifetime), so the
  launcher's own remote sub-resources (thumbnails) are direct until then. The
  launcher loads only this app's UI from the local asset loader, so this is a
  timing note, not a policy hole: a site page cannot run before the first open.
* **Plain HTTP** stays refused everywhere (navigation policy + proxy 405), so
  the proxy never carries a cleartext request.
* **HTTP/3 / QUIC.** An HTTP CONNECT proxy carries TCP tunnels; Chromium does
  not use QUIC for a proxied request. That is a property of the engine, not of
  this code, and it is on the device checklist as a packet-capture item.
* **Other apps on the device.** The listener is `127.0.0.1` only, so it is not
  reachable from the network, but another app on the same device could connect
  to it. Doing so gains that app nothing it could not do directly (the proxy
  only reaches public addresses and holds no credentials, and it never sees
  plaintext), and it cannot make the app's WebViews bypass the proxy.
* **A process crash** leaves the OS with no override once the process is gone;
  a new process starts unpinned and refuses to open a site until it installs
  the override again. That is the intended fail-closed path, not a gap.

### The old pre-check is now defence in depth, not the guarantee

`NavigationDnsGate` and `SiteNavigationPolicy` are unchanged and still run:
they refuse IP literals, private host suffixes and (cached) private or flipping
answers before a request is attempted, and they remain the only protection that
can give a *synchronous* answer on the UI thread. They are now the second line
behind a check that cannot be raced. Their comments and this document say so;
none of them is described as connection pinning anymore.

## Round 15 — what this round fixed, and what it did not

### CI: rustfmt and the Vite advisory (both verified, nothing loosened)

`cargo fmt --all -- --check` failed in the Ubuntu, Windows and macOS jobs of
run `39662014373` in `anilist.rs`, `commands.rs` and `error.rs`. The formatting
was applied to the sources; the check itself, the failure exit code and every
workflow condition are untouched.

`npm audit` failed on `vite 8.3.1`. Six advisories (`GHSA-698v-4r3x-3vcv`,
`GHSA-r825-w8mm-5pxw`, `GHSA-587h-7wmj-h5pm`, `GHSA-9jrq-w75r-8gcw`,
`GHSA-rq7h-c2jc-7f22`, `GHSA-vfpm-58rq-9qcg`) cover `vite >=8.3.0 <=8.3.2` with
`first_patched_version 8.3.3`, so the fix is a patch bump inside the existing
`^8` range: `package.json` now says `^8.3.3` and the lock resolves `8.3.5`
(current stable). No major upgrade, no `npm audit fix --force`, no ignored
advisory and no workflow change.

### The two CI gates that only started running once the format check passed

`Rust format` runs before `Clippy` and `npm audit` runs before `cargo audit`, so
failing the first step hid the second in every earlier run. Fixing the format
and the Vite advisory therefore surfaced two more failures, both real:

* `cargo clippy --all-targets -- -D warnings` reported `push_android_blocklist`
  and `close_active_android_site` as never used, plus three `needless_borrow`
  hits. Both helpers were gated on `#[cfg(any(target_os = "android", test))]`
  but are only called from the `#[cfg(target_os = "android")]` branch of
  `sync_android_blocklist`, so on a host `--all-targets` build they were
  compiled and unreachable. They are now Android-only (the non-Android fallback
  bodies were stubs no test could observe). The borrow hits were `&read_live`,
  `&record`, `&no_close` in the transfer-ordering test: those closures capture
  only shared references, so they are `Copy` and the borrows were redundant.
  No behaviour change, and no `#[allow]` was added to silence anything.
* `cargo audit --file src-tauri/Cargo.lock` reported two advisories from the
  same CA-distrust event (19–20 October 2026, `CVE-2026-111943`):
  `RUSTSEC-2026-0381` for `webpki-roots 1.0.9` (fixed in `1.0.10`) and
  `RUSTSEC-2026-0382` for `rustls-webpki 0.103.15` (fixed in `0.103.16`).
  Both are patch bumps inside the ranges their dependents already allow
  (`rustls 0.23.46` wants `^0.103.14`; `reqwest`/`hyper-rustls` want `^1`), and
  neither changes a resolved dependency in `Cargo.lock` — verified against the
  crates.io index entries for the two target versions, including their
  checksums. Both crates decide which roots a TLS connection may trust, so this
  is the one place where the patch bump is a security fix rather than hygiene.

`cargo audit` also prints eleven `unmaintained`/`unsound` informational warnings
(`glib`, `zvariant`, `unic-*`, `ring`, `proc-macro-error`, `zbus_names`) that
come from the Tauri/Linux stack and do not fail the gate; they are not
vulnerabilities and none of them was silenced.

### Android DNS gate: two real defects, and what is still open

The call chain was re-verified against the WebView contract rather than the
comments:

* `shouldOverrideUrlLoading` covers main-frame navigations and main-frame
  server redirects on API 24+ (`WebResourceRequest.isRedirect()` exists for
  exactly that reason); it may also be called for subframes. It must answer
  synchronously, so a cache miss returns `ALLOW_AND_PROBE`.
* `shouldInterceptRequest` covers sub-resources and frames, but the platform
  documents that **for redirects it is only called for the initial URL, not
  the subsequent redirect URLs**, and that it is **not** called for
  `javascript:` or `blob:` URLs. That is precisely why the top-level
  checkpoint consults the shared verdict cache instead of trusting the request
  checkpoint to see every hop.
* `WebView.loadUrl` (the first navigation in `buildWebView`) bypasses
  `shouldOverrideUrlLoading`; it is covered by the policy check in
  `open_on_mobile` before the view is built, not by the guard.

Two defects were found and fixed in `NavigationDnsGate`:

1. **An empty resolver answer was cached as a public verdict.** `record(host,
   emptyList())` classified as ALLOW and stored `Verdict.PUBLIC` with an empty
   `pinned` set, so the next top-level navigation was allowed *from the cache*
   for a host whose address was never established. A "no answer" is not
   evidence of a public address: `record` now writes no entry at all (the
   host's probe slot is still released) and the next navigation probes again.
   Regression test: `an empty resolver answer is never cached as a safe
   verdict` (fails on the previous code, which answered `PUBLIC`).
2. **The probe set, and therefore the resolver queue, was unbounded.**
   `probing` grew with every distinct hostname a page navigated through, and
   each entry queued a task on the guard's single-thread executor, where it
   blocks for the resolver timeout. `NavigationDnsGate` now caps the set at
   `MAX_PROBING` (32); a host that cannot get a slot is simply not probed,
   which is the same "unknown, fail open" state as a cache miss (the request
   checkpoint still resolves synchronously). Regression test: `the probe budget
   bounds the resolver queue and recovers when a slot frees`.

One resource leak was found and fixed: `SiteNavigationGuard` owns
`Executors.newSingleThreadExecutor`, and the controller creates one guard per
site open, but nothing called `shutdown`. Each open therefore leaked one daemon
resolver thread for the life of the process. The guard now has `close()`, and
every teardown path (`finishClose`, the failed-open rollback, `destroy`,
including its no-view early return) releases the guard next to
`webView.destroy()`.

**R-01 (b) — superseded by round 16 (below).** None of *this* round pins the
address used for the connection: the engine still re-resolves when it connects,
so a rebinding host can answer public to both checkpoints and private to the
socket. Round 16 replaces the decision with a loopback proxy that owns the
connection; until a device run confirms it, treat the note in this section as
the accurate description of the pre-check alone. The probe cap
and the empty-answer rule make the pre-check honest, not complete. The required
fix (a network layer that owns the resolution, e.g. the evaluated
`PROXY_OVERRIDE` CONNECT proxy) and the device matrix it needs are unchanged —
see [DNS rebinding on Android](#dns-rebinding-on-android). No test in this
round is a substitute for that matrix.

### Blocklist: a rebuild could install older rules under a newer generation

`rebuild_blocklist` read the settings and installed the matcher in two separate
steps. `update_settings` and `reset_blocklist` are synchronous commands (Tauri
runs them on the main thread), but `set_pip_auto_enter` is
`#[tauri::command(async)]` and calls the same function from the async runtime.
Interleaving: A reads rules R1, the user saves R2 and B installs R2 at
generation 5, A resumes and installs R1 at generation **6**. The delivery guard
then pushes R1 as the newest snapshot and `applied = 6` keeps R2 out for good.

The read+install pair now runs under `BlocklistSyncGate::lock`, so rebuilds are
serialized against each other and against transfers; the generation order is
the settings-read order again. Lock order is `gate` → `settings` → `blocklist`
and `gate` → `ANDROID_SESSION_LOCK`; nothing takes the gate while holding
settings, the matcher or the session lock, so no cycle is added. Regression
test: `rebuild_blocklist_cannot_install_while_a_transfer_holds_the_gate`
(asserts no install happens while another thread holds the gate, and that
exactly one install happens afterwards).

Both of `rebuild_blocklist`'s locks used `lock().expect(..)`, i.e. the exact
panic-on-poison trap `lock_recover` was introduced to remove: a panic elsewhere
while holding `settings` or `blocklist` used to make every later open/settings
change panic. They now go through `lock_recover`; regression test
`rebuild_blocklist_survives_poisoned_settings_and_blocklist_locks`.

### Documentation corrected

`AnimeHubSiteController`'s header claimed the one-time migration requires
"every `setCookie` callback accepted", which contradicts the implementation and
the section above: `SiteMigrationFlow` skips an individually rejected cookie
(usually a foreign Domain a site profile cannot own) and treats a durable
`flush()` as the success criterion. The header now says what the code does.

### Three gates that had never run, and the one real bug behind them

`Rust tests` runs after `Clippy`, which runs after `Rust format`, and the
format step had been failing for several rounds, so `cargo test` had not run at
all. Its first run (322 tests) found 9 failures — six on the first pass and
three stale `anilist` expectations on the next — and all of them are fixed:

* **Real bug.** `cookie_belongs_to_site` treated a *blank* `Domain` attribute as
  a host-only cookie: `.filter(|d| !d.is_empty())` turned `Some("")` into the
  `None` arm, which returns `true` for any site. RFC 6265 forbids an empty
  `Domain`, so this was a malformed cookie being treated as owned by whatever
  site the migration was for — the exact case the function guards. Only `None`
  is host-only now; a blank or dot-only value falls through and is refused.
* Stale assertions: a site URL outside the test jar's cookie scope
  (`https://a.example/` vs `.example.com`), and an equality check against
  `"open:site-"` where profile names are `site-<32 hex>-g<gen>`.
* The three `anilist` failures were stale in the other direction: `loopback_port`
  was deliberately narrowed to exactly `http://127.0.0.1:<port>/callback`
  (`localhost` can resolve to `::1`, an omitted port means 80, and the listener
  binds only `127.0.0.1:<port>` on `/callback`), and the refusal text no longer
  contains the word "loopback". The tests now assert that contract, including
  that the refusal names the only accepted form and never echoes the configured
  redirect.

### OAuth: a pending state that no longer had a listener

`anilist_login_start` writes the `state` value and then serves the loopback
callback from a background task. When that task failed — the port was taken, or
no callback arrived inside the listener's 300 s window — it reported the error
and exited, leaving the state armed in the slot. This is not exploitable by
itself (`take_pending_state` consumes only the *current* value, so a leftover
cannot be replayed with another attempt's `code`), but it made "a login is
pending" untrue and would have masked a future regression where the slot is
read more broadly. The failure path now calls `abandon_pending_state`, which
clears the slot **only if it still holds this attempt's value** — a login
started in the meantime keeps its state. Regression test:
`abandoning_a_pending_state_clears_only_its_own_attempt`.

### Actions pinning (R-09) is still blocked, not fixed

The active workflows still use mutable tags. `workflow_dispatch` is also
refused for this token (`HTTP 403: Resource not accessible by integration`), so
CI can only be triggered by a pull request. Nothing in this round changed a
`.github/workflows/` file; see
[Actions pinning — explicit blocker](#actions-pinning--explicit-blocker).

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
**active** `.github/workflows/*.yml` still reference mutable tags. This was
re-attempted in the current round — the pinned files were committed on top of
the branch and pushed — and GitHub rejected the push again with:

```
refusing to allow a Personal Access Token to create or update workflow
`.github/workflows/ci.yml` without `workflow` scope
```

The rejection is a hard server-side rule, not a transient failure: the token
authenticates (`GET /user` → `mystery2430`) and the repository reports
`push`/`admin` for it, and a commit that touches **no** workflow file was
accepted on the same branch in the same round. Only `.github/workflows/` content
is refused, which is exactly the fine-grained-PAT `Workflows` permission.

The permission was not worked around, no credential was changed, nothing was
pushed to `main`, and no tag or release was created. The rejected commit was
rolled back locally so the branch never carried workflow edits the token cannot
land, and the tree therefore still matches the docs. The item stays **blocked**,
not fixed.

Every SHA below was resolved from its tag through
`GET /repos/<owner>/<repo>/commits/<tag>` during this round, so a maintainer
does not have to re-derive them. They match the `ci-fixes/` proposals exactly —
the proposals were **verified**, not assumed correct:

| Action | Verified commit SHA | tag |
| --- | --- | --- |
| `actions/checkout` | `3d3c42e5aac5ba805825da76410c181273ba90b1` | v7 |
| `dtolnay/rust-toolchain` | `b725eda525362e7cf34af9a7243cac1e3f01a7ec` | stable |
| `Swatinem/rust-cache` | `d1527c8f67ef9af357290475d4af4b0fab8f12e0` | v2 |
| `actions/setup-node` | `949feb2413d6458794dcd2491c4babbbce0c15c1` | v7 |
| `actions/setup-java` | `b843ebf9b913146b38da2eaa7aeddf76074ae1ad` | v6 |
| `android-actions/setup-android` | `be39fa834029ff78f1a44aa3bb0819b8fc2bd8fd` | v4 |
| `actions/upload-artifact` | `cf430e030ddbb5b0abf93d22962f4752f3646cd9` | v7 |
| `actions/download-artifact` | `9000827ccba6bdab643e8b6fd33ac0654aef8333` | v8 |
| `tauri-apps/tauri-action` | `1deb371b0cd8bd54025b384f1cd735e725c4060f` | v1 |

36 `uses:` references across `ci.yml` (19), `release-android.yml` (7),
`release-desktop.yml` (6) and `publish-release.yml` (4 external; its two
`uses: ./.github/workflows/...` are in-repo reusable workflows and are not
pinnable). `tests/command_wiring.test.js` asserts this table, so the values are
in a tested location even while the workflows themselves cannot be changed.

Note on `dtolnay/rust-toolchain@stable`: pinning freezes the *installer* action
at a commit, not the Rust toolchain — the job still installs whatever `stable`
is that day. That is the correct reading of the pin, not a reproducibility
claim.

**The exact change a maintainer with `workflow` scope has to make** — replace
each `owner/action@tag` with `owner/action@<sha> # tag` in the four ACTIVE
files, and nothing else:

```
actions/checkout@v7                  -> actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7
dtolnay/rust-toolchain@stable        -> dtolnay/rust-toolchain@b725eda525362e7cf34af9a7243cac1e3f01a7ec # stable
Swatinem/rust-cache@v2               -> Swatinem/rust-cache@d1527c8f67ef9af357290475d4af4b0fab8f12e0 # v2
actions/setup-node@v7                -> actions/setup-node@949feb2413d6458794dcd2491c4babbbce0c15c1 # v7
actions/setup-java@v6                -> actions/setup-java@b843ebf9b913146b38da2eaa7aeddf76074ae1ad # v6
android-actions/setup-android@v4     -> android-actions/setup-android@be39fa834029ff78f1a44aa3bb0819b8fc2bd8fd # v4
actions/upload-artifact@v7           -> actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9 # v7
actions/download-artifact@v8         -> actions/download-artifact@9000827ccba6bdab643e8b6fd33ac0654aef8333 # v8
tauri-apps/tauri-action@v1           -> tauri-apps/tauri-action@1deb371b0cd8bd54025b384f1cd735e725c4060f # v1
```

Verified property of that change: `git diff -U0 -- .github/workflows/` touches
72 lines and **every one of them is a `uses:` line**, so triggers,
`permissions:`, the `publish-release` gate conditions
(`head_branch == 'main'`,
`head_repository.full_name == github.repository`), `environment: keystore` and
every artifact step stay byte-identical.

Two things a maintainer should know before choosing how to apply it:

* `ci-fixes/apply.sh` copies the whole proposal files, which is **not** the same
  as the minimal change above. `ci-fixes/ci.yml` additionally inserts a
  `Verify resolved Android pins` Gradle step asserting the resolved graph holds
  exactly `androidx.webkit:webkit:1.16.0` and `kotlin-gradle-plugin:2.1.20`.
  Those are the versions `scripts/android_prepare.py` pins, so the step is
  plausible — but it was **deliberately not applied** here, because no
  Gradle/JVM/Android SDK exists in this environment and a strict assertion
  nobody could run is an unverified change to the Android CI job. Land it
  separately, after one real run.
* After the pinning lands, two tests must move with it: `tests/gaps.test.js`
  (`assert.match(yml, /actions\/upload-artifact@v7/)`, and the pinning scan that
  currently only walks `ci-fixes/`) and the
  `"Actions pinning is reported as a blocker, not claimed as fixed"` case in
  `tests/command_wiring.test.js`, which asserts the active `ci.yml` is *not*
  pinned. Both say so in their own failure messages.


## Partially mitigated / residual

### DNS rebinding on Android (R-01 b)

**Status: mitigated by construction on Android site views since round 16;
still OPEN pending a device run.** The connection decision moved into an
in-process loopback proxy (see
[Round 16](#round-16--dns-rebinding-is-decided-where-the-connection-is-made)),
so a site WebView no longer has a resolver of its own to be raced. What is
missing is not code or tests but observation on a real WebView engine:
whether the engine sends `CONNECT` for every HTTPS/WSS request, whether QUIC
is suppressed under an override, and whether the launcher and IPC still work.
Those are the checklist items in
`docs/android-dns-pinning-device-checklist.md`. Until they pass, this finding
must not be reported as `fixed`.

The two WebView-callback checkpoints (`NavigationDnsGate` +
`SiteNavigationPolicy`) are retained unchanged **as defence in depth**. They are
no longer presented as the guarantee, and their own documentation says so:

* they refuse any non-empty answer containing a private, loopback, link-local,
  reserved or malformed address;
* they refuse a public answer that **differs** from the first pinned public set
  for that host (flip detection);
* they refuse all IP literals and built-in host suffixes;
* an empty resolver answer writes no verdict and in-flight probes are capped at
  `MAX_PROBING`, so neither the verdict set nor the single resolver queue grows
  with a page's host count;
* a verdict lasts `CACHE_MS` (60 s), after which the host is unknown and is
  re-probed; the request checkpoint re-resolves synchronously, so an expired
  verdict cannot turn a known-bad host into an allowed connection.

Their residual is the **TOCTOU** window: the engine resolves again when it
connects, so a rebinding server could answer a public address to both
checkpoints and a private one to the socket. That window is what the proxy
removes — by owning the connect, not by adding a callback. A resolver outage
remains deliberately fail-open inside `NavigationDnsGate` so an offline DNS
server does not brick the app; the proxy is the component that turns an empty or
broken answer into a refusal (502) instead.

What remains genuinely open on Android, stated as open:

* **Desktop has no equivalent.** The Tauri desktop child WebView offers no
  supported network hook; `src-tauri/src/web/dns.rs` + `decide_navigation` keep
  the same pre-check shape and the same TOCTOU residual. Unchanged here.
* **Provider coverage is unverified.** A provider without `PROXY_OVERRIDE`
  cannot be pinned, and the app refuses to open a site there rather than
  degrading. That is a fail-closed availability cost, not a security hole, but
  which providers are affected is a device-matrix question.
* **The launcher's first remote sub-resource** (before any site has been
  opened, hence before the override is installed) is direct. The launcher only
  renders this app's own local UI, so no untrusted page exists at that moment.

#### Alternatives that were rejected

Researched against the platform APIs rather than assumed:

* **`androidx.webkit.ProxyController` is the only supported WebView network
  hook** — `WebViewFeature.PROXY_OVERRIDE`, `getInstance()` throws
  `UnsupportedOperationException` otherwise. This is the approach now used, and
  the reasons it is the right one (single resolver for check and connect,
  no TLS termination, every redirect hop a fresh `CONNECT`) are in the round-16
  section.
* **`shouldInterceptRequest`-side fetching** would mean reimplementing the
  network stack (streaming video, range requests, `CookieManager` sync, HTTP
  auth, service workers) and is not called for every main-frame navigation.
* **Chromium's `--host-resolver-rules`** is not settable on Android WebView.
* **`ConnectivityManager.bindProcessToNetwork`** selects a network; it does not
  filter destinations.
* **`VpnService`** filtering needs user consent for a VPN and is a different
  product decision.
* **Tauri's Rust HTTP client** never carries WebView traffic, so nothing in
  `src-tauri/src/web/dns.rs` can pin a WebView connection.

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
