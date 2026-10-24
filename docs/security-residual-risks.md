# Residual security risks after PR #27 fixes

This document records what this branch fixes and what it deliberately does
**not** claim to fix.

Sections below are kept in the order they were written, and they are **history**:
each one describes the state at the end of its round, including status lines
that a later round made obsolete. The next section is the only one that
describes the branch as it stands now.

## Round 19 — current state of the branch

Round 19 found a real defect in the Android resolver adapter — the shipped code
did not do what its own tests and documents said it did — and settled, with
source evidence, how far the platform-slot accounting can be trusted. It also
answers the desktop first-request question per platform. No architecture
changed; nothing here is device evidence.

### 1. The defect: a post-cancel platform report was swallowed

`PlatformDnsLookup.start()` used one `AtomicBoolean("settled")` for two
different things: "the platform has reported" and "the caller asked to cancel".
`Cancel` set that flag before calling `CancellationSignal.cancel()`, so a
report that arrived afterwards was dropped by the callback. Consequences:

* `AsyncDnsResolver.complete()` never ran for a cancelled query, so
  `platform.remove(...)` never happened and the slot was reclaimed only by the
  **60 s watchdog** — for a query that had actually finished;
* the resolver's own comment ("a late answer … still releases the platform
  slot") was true of the resolver and false of the adapter in front of it;
* the JVM tests could not see it: `ManualLookup` forwarded every callback,
  i.e. the fake was *more permissive* than production. A test that models a
  different contract than the shipped code is worse than no test.

**Fix.** The delivery rule is now a separate, pure-JVM class,
`DnsAnswerDelivery`, used by `PlatformDnsLookup` and driven directly by the
tests. It has exactly one job: hand the platform's **first** report to the
resolver, whatever the cancellation state (`onAnswer` then `onError`, or a
duplicate answer, still deliver once). Cancellation is now only "ask the
platform to stop" and never clears the delivery latch, so a report that
arrives after the caller gave up still reaches `complete()`, which refuses to
settle the finished flight but does release the slot. The tests gained an
`AdapterLikeLookup` that routes its reports through that same class, so the
two "late report frees the slot" tests exercise the production rule rather
than a lookalike.

### 2. What the 60 s number actually is: a watchdog, not proof

Round 18 presented the two ceilings as "logical flights" and "platform queries
whose callback has not arrived", which reads like a hard bound on platform
work. The evidence does not support that.

**Public contract** (developer.android.com, fetched 2026-10-24):

* `DnsResolver.query(..., CancellationSignal, ...)`: the signal is "used by
  the caller to signal if the query should be cancelled. May be null." There
  is no statement about what happens to the callback afterwards.
* `CancellationSignal.cancel()`: "Cancels the operation and signals the
  cancellation listener. If the operation has not yet started, then it will
  be canceled as soon as it does."
* `CancellationSignal.setOnCancelListener()`: "This method is intended to be
  used by the recipient of a cancellation signal such as a database or a
  content provider to handle cancellation requests while performing a
  long-running operation. This method is not intended to be used by
  applications themselves." — that is a statement of intent about the API's
  audience, **not** a prohibition; `DnsResolver` is the intended recipient and
  installs the listener itself, while an application calling `cancel()` is the
  normal use of the signal.
* `DnsResolver.query` (API 29 overload): "If at least one query succeeded with
  valid answer, rcode will be 0" — the basis for treating any non-zero `rcode`
  as a failure (round 18's `dnsAnswerOrFailure`, unchanged).

**AOSP implementation** (API 30 SDK sources, `android/net/DnsResolver.java`,
fetched 2026-10-24):

* `cancelQuery(fd)` removes the fd event listener from the main looper's queue
  and calls `resNetworkCancel(queryfd)`, whose comment is "Closes fd, marks it
  invalid."
* the fd listener's executor task re-checks `cancellationSignal.isCanceled()`
  under the query lock and returns **without** calling the callback if the
  signal fired;
* therefore, in that implementation, cancel usually ends the query with no
  callback — but a task that already passed the `isCanceled()` check can still
  call `onAnswer`, and nothing in the API tells the caller which happened.

**Conclusion (design choice B).** A callback always finishes the accounting at
once; the 60 s value is only the fallback for a query that never reports, and
it removes *our bookkeeping*, not platform work. The honest statement is:

> At most 8 queries are **tracked** as unfinished. A report frees its slot
> immediately; a query that never reports is no longer tracked 60 s after the
> cancel. Nothing in the public API proves the platform stopped working on it,
> so "at most 8 running platform queries" is **not** claimed.

This is an availability residual, never a security one: a report that arrives
after the flight settled cannot be used (the same `complete()` refuses it),
no address from an untracked query is classified, and nothing is cached.

### 3. Desktop first request, per platform (round 19 answer)

Question: is there a **supported** API, available to this app, that prevents
the first network request from leaving when the proxy setting was not applied?

| Platform | Supported pre-request veto? | Evidence |
| --- | --- | --- |
| Windows / WebView2 | **Partly, and not through our stack.** `WebResourceRequested` + a resource filter runs before the request is sent and can answer it from the host. But `AddWebResourceRequestedFilter` is documented as deprecated and as not behaving "as expected for iframes"; the newer `AddWebResourceRequestedFilterWithRequestSourceKinds` does not fix WebSocket handshakes, which have no interception event. wry 0.55.1 does not expose either; it would need `with_webview` + raw COM per view. | Microsoft Learn, `ICoreWebView2::AddWebResourceRequestedFilter` (deprecation + iframe note), `add_WebResourceRequested` |
| Linux / WebKitGTK | **Yes for resource loads, but out of reach of this stack.** `WebKitWebPage::send-request` is emitted "when request is about to be sent to the server" and returning `TRUE` cancels the load. It belongs to the **web-process extension** API (a separate `.so` loaded by the web process, registered on the context); wry sets up its own context and exposes no hook for it, and the signal does not cover WebSocket handshakes. | webkitgtk.org `WebKitWebPage` reference, `send-request`; WebKit bug 83681 (API added for the injected bundle) |
| macOS / WKWebView | **No.** `WKNavigationDelegate` covers navigations (top frame and iframes), not `fetch`/XHR/subresources; `WKURLSchemeHandler` only serves custom schemes; `WKContentRuleList` blocks by URL pattern, cannot ask "did the proxy apply?", and is not a network veto. | Apple WebKit API surface (no subresource interception delegate) |

None of these is implemented in this round, and none of them would be a
"comment fix": each is a new native component with its own gaps (WebSocket,
iframes, per-view wiring). The proxy binding and the post-load zero-CONNECT
check are what this branch has; they are **not** a first-request guarantee, and
the residual stays **Open** (P1: *Implemented; device verification pending*).

### 4. Status after round 19

| Item | Status | Change this round |
| --- | --- | --- |
| Android DNS cancellation (P2) | **Implemented; device verification pending** | The adapter now forwards the first platform report after a cancel, so a real completion frees its slot; 22 JVM tests (7 new) drive `DnsAnswerDelivery` and an adapter-shaped fake. The platform-query ceiling is documented as a watchdog, not a bound on platform work. |
| Desktop proxy / first request (P1) | **Implemented; device verification pending**; first-request residual **Open** | Per-platform answer above; no new mechanism. |
| Loopback proxy auth (P3) | **Open; product decision** | Unchanged; no 407 code. |
| WebRTC/UDP (P4) | **Partially fixed**; UDP/QUIC **Open; technical limitation** | Unchanged. |
| AniList OAuth (P5) | **Implementation fixed; AniList registration/E2E pending** | Unchanged. |

**What is still not proven:** whether a real provider honours the cancel, and
whether it ever reports afterwards (checklist items 36–39 and 42 remain NOT
YET RUN); a real WebView2/WebKitGTK/WKWebView run for P1; an AniList login; and the
independent human review. R-01(b) stays open. The Action pins (R-09) still live
only on this branch.

## Round 18 — state at the end of round 18 (history; corrected by round 19)

Round 18 is a verification round: it closed what the source could prove, and
narrowed every claim it could not. No re-architecture, no new mechanism, and no
item declared closed by a test or a comment alone.

| Item | Status | What is true now | Evidence | What is still not proven |
| --- | --- | --- | --- | --- |
| Desktop proxy (P1) | **Implemented; device verification pending** | Windows: `additional_browser_args` carries wry 0.55.1's own default string plus `--proxy-server=http://127.0.0.1:{port}` and `--proxy-bypass-list=<-loopback>`; it is `cfg`-exclusive with `proxy_url` (setting both would make wry drop the proxy). Linux: wry sets the proxy on the `WebContext` website-data-manager **before** the first `WebView` is created. macOS: wry sets `proxyConfigurations` on the `WKWebsiteDataStore` **before** the webview is initialised, behind the `macos-proxy` default feature; the app's Darwin ≥ 23 (macOS 14) gate matches wry's `custom_data_store_available`. | `tests/desktop_proxy_wiring.test.js` (8 source-string pins), `pinned_proxy.rs` 12 tests, three desktop CI jobs. | A real WebView2 / WebKitGTK / WKWebView. **Scope is not per-view:** the Linux proxy belongs to the `WebContext` and the macOS one to the data store, so every view sharing them uses it. |
| Desktop fail-closed check (P1) | **Implemented; post-load only** | `site_proxy_requests() == Some(0)` runs inside `PageLoadEvent::Finished`: a page that finished loading with zero proxy CONNECTs closes the site and tells the UI. | `windows.rs` comment + the `Finished`-guard pin in `tests/desktop_proxy_wiring.test.js`. | **It is not a first-request guarantee.** The check can only run after the load, so a provider that ignores the setting has already fetched the document. No pre-request veto exists in wry 0.55.1 or in the three WebView APIs reviewed; the residual stays **Open**. |
| Android DNS cancellation (P2) | **Implemented in code; device verification pending** | The lookup runs on `DnsResolver.query` with a caller-owned deadline; the caller is released at that deadline whatever the provider does. A late or duplicate answer can free its slot but can never settle a finished call. Per-host single-flight. **Two independent ceilings of 8** — logical flights being awaited, and platform queries whose callback has not arrived — both refuse instead of queueing. A cancelled query's platform slot is released by its callback or, failing that, 60 s after the cancel (swept on the next CONNECT). `rcode != 0` is a resolution failure, never an address list. | `AsyncDnsResolverTest` (15 JVM tests, fake lookup: before/at/after the deadline, throwing cancel, stale answer for the same host, grace-period reclaim, rcode, ceilings) and `DnsPinnedProxyTest`, in the Android CI job. | Whether a real provider honours `CancellationSignal` and whether it ever calls back. The reference promises neither and gives no post-cancel ordering, and it says the listener is "not intended to be used by applications themselves" — a statement about the API's audience, not a prohibition (round 19 §2 corrects the earlier "forbids" wording); `PlatformDnsLookup` itself has never run on a device. The 60 s figure is bookkeeping, not proof that the platform stopped (round 19 §2). Checklist items 36–39 remain **NOT YET RUN**. |
| Loopback proxy authentication (P3) | **Open; product decision** | The listener is loopback-only, ephemeral, public-destination-only and never cleared, but unauthenticated: `ProxyConfig.Builder` offers no credentials, and no `Proxy-Authorization` can be attached to the WebView's own `CONNECT`. | `docs/security-residual-risks.md` §4 (another app can reach and use it). | Any real-WebView evidence that a `407` challenge would be answered. Credentials would have to be random, native-only and unlogged. Smallest next step: a device probe that points a WebView at a loopback proxy answering `407` and records whether the `CONNECT` completes. Until then: no 407 code. |
| WebRTC / UDP (P4) | **Partially fixed**; UDP **Open; technical limitation** | Only the JS API is restricted (`WEBRTC_GUARD` at document start on Android, all frames on desktop), and a provider without document-start support refuses the open. There is **no OS-level UDP/QUIC block**; `VpnService`/TUN is out of scope without a product decision. | Source pins for the guard; the refusal path is covered by the sentinel tests. | The iframe case, `RTCPeerConnection` delete/redefine, and a UDP-443 capture (checklist items 40–41 after the round-19 renumbering, not run). "UDP is pinned/proxied" is never claimed. |
| AniList OAuth (P5) | **Implementation fixed; AniList registration/E2E pending** | Both platforms accept only `http://127.0.0.1:<port>/callback`: `prepare_login` calls `validate_redirect_uri(&cfg.redirect_uri, false)` and the loopback listener is bound before the authorization URL is built; a custom scheme is refused first. The old build-manifest `animehub` intent filter is still generated by `scripts/android_prepare.py` and remains inert for OAuth — it was **not** "deleted". | `prepare_login_refuses_the_custom_scheme_on_every_platform`, `tests/command_wiring.test.js`. | A real AniList application registered with the exact redirect URI and a full login run. The client id is not in the repository, so no E2E login has happened. |

**What round 18 changed in the source** (each one is a narrow, reviewable commit):
the Android resolver's cancellation and `rcode` handling
(`AsyncDnsResolver.kt`), its JVM tests, the stale resolver KDocs in
`DnsPinnedProxy.kt`, the post-load wording of the desktop zero-CONNECT check and
its pins. P3–P5 were documentation-only corrections: no code changed for them,
because there is no verified mechanism to add. Nothing here changed the proxy's
decision logic, the IP classification, or the loopback bind.

**CI for round 18:** run **40123086415 @ `728b129`**, the last commit of the
round that changed code — all six jobs
(`Tests` on ubuntu-24.04 / macos-latest / windows-latest, `Release profile
(LTO)`, `Android compile (aarch64)`, `Dependency and secret audit`) completed
successfully; every later commit in the round is documentation-only and is
validated by the same matrix on its own push. That proves compilation and
the host suites; it is not device evidence, and the Android job's JVM suite
uses a fake lookup for the resolver, so it cannot show provider behaviour
either.

**Still open after round 18, stated narrowly:** a real desktop WebView and a real
Android provider run (every "Implemented" above is device-verification-pending);
loopback reachability by other apps (GO-01); WebRTC/UDP (GO-04); DNS rebinding on
desktop and the `Unknown` fail-open path (GO-02/03); R-09's `main` aspect (the
pins live on this branch only); fixed-port OAuth squatting (GO-06); R-01(b) and
the independent human review; checklist items 26–40.

## Round 17 — state at the end of round 17 (history; superseded by round 18/19)

This table described the branch at the end of round 17. It is kept as history:
the round-18 section above it is the current state. Everything after this point
is a round log.

**What is implemented and exercised by automated tests on every push**

| Area | What exists now | Evidence | What is still not proven |
| --- | --- | --- | --- |
| Desktop DNS pinning | A Rust CONNECT-only proxy (`src-tauri/src/web/pinned_proxy.rs`) bound on an ephemeral `127.0.0.1` port **before** the child WebView exists, installed per session through wry 0.55.1 (`additional_browser_args` with `--proxy-server` + `--proxy-bypass-list=<-loopback>` on Windows, `WebviewBuilder::proxy_url` on Linux, the `macos-proxy` feature on macOS). Fail-closed: the open returns an error if the bind fails, and a load that **finished** without the proxy seeing a single CONNECT closes the site (`PageLoadEvent::Finished`, i.e. after the fact — not a first-request guard). | 12 Rust tests in `pinned_proxy.rs` — 11 real-socket tests (public tunnels, rebinding refused, mixed answer refused whole, private/empty/NXDOMAIN refused, literals never resolved, >8 KiB head refused, unreachable destination refused, `stop()` kills the tunnel, two starts use different ports, one resolution per CONNECT) plus 1 parser test — `tests/desktop_proxy_wiring.test.js` (source-string pins only); the three desktop CI jobs. | A real WebView2 / WebKitGTK / WKWebView run. A JVM/Rust test and a green CI job are not that. |
| Android DNS pinning | `SiteProxyController` installs a process-wide `PROXY_OVERRIDE`; `DnsPinnedProxy` resolves once per CONNECT, classifies every answer and connects to the classified `InetAddress`; `AsyncDnsResolver` moves the lookup onto `DnsResolver.query` with a 10 s caller-owned deadline, drops late answers and refuses instead of queueing above its in-flight ceilings (round 18 rewrote this path; see the round-18 section for the current guarantees). | `DnsPinnedProxyTest`, `SiteProxyGateTest`, `AsyncDnsResolverTest` (host JVM, real sockets where the proxy is involved) run in the Android CI job. | Whether the provider honours `PROXY_OVERRIDE` at all, and whether cancelling actually frees the platform thread. Device matrix unchanged: [`android-dns-pinning-device-checklist.md`](./android-dns-pinning-device-checklist.md). |
| WebRTC / UDP | Android: `WEBRTC_GUARD` is installed with `addDocumentStartJavaScript` (the round-16 `onPageStarted` fallback, which ran after the page could already open a peer connection, is gone). Desktop: `initialization_script_for_all_frames`. | Source pins in `src-tauri/android-plugin/src/lib.rs` and `session.rs`. | Every part of it. The guard is page JavaScript, not OS-level UDP filtering; QUIC/UDP needs a packet capture; the Android provider must support the document-start API or the open is refused (fail-closed, see below). |
| OAuth return path | Loopback only, on **every** platform: `prepare_login` calls `validate_redirect_uri(&cfg.redirect_uri, false)`, so `animehub://anilist/callback` is refused before a listener, a state value or an authorization URL exists. | `prepare_login_refuses_the_custom_scheme_on_every_platform`, `tests/command_wiring.test.js`. | Nothing on this side; the login still needs a manual run against AniList because the client id is not in the repo. |
| Actions pinning (R-09) | All 36 external `uses:` references in the active workflows are pinned to reviewed 40-character SHAs; `ci-fixes/` remains a reference copy that GitHub does not execute. | `tests/gaps.test.js`, `tests/command_wiring.test.js`; the six-job matrix has been green on this branch (first: **40108690774 @ `7948e8f`**, latest before round 18: **40120340164 @ `87ba782`**), and PR #30 carries the runs for the round-18 heads (**40123086415 @ `728b129`**, 6/6, plus the documentation-only heads). | `main` is untouched: the pins live on this PR branch until it is merged. |

**What a green CI run does not mean.** The six jobs are `Tests` on
ubuntu-24.04 / macos-latest / windows-latest, `Release profile (LTO)`,
`Android compile (aarch64)` and `Dependency and secret audit`. None of them runs
a WebView, a device, an emulator or an AniList login. A green run proves the
code compiles, the host-side suites pass and the profile links — it is not
evidence about provider behaviour.

**Still open, by kind**

* **Open; technical limitation** — WebRTC/ICE/STUN/TURN/UDP are outside the
  pin. The document-start guard is defence in depth inside the page; the only
  layer that could actually filter that traffic is the OS (`VpnService`/TUN),
  which is a product decision, not a fix.
* **Open; product decision** — loopback proxy authentication, and whether to
  ship `VpnService`/TUN for UDP.
* **Open; independent review needed** — everything marked "device verification
  pending" above, and the review itself (R-12's exclusion stands).
* **Open** — other applications on the same machine can reach the loopback
  proxy port: an ephemeral port is not authentication, and the `407` prototype
  in `docs/security-residual-risks.md` is not shipped.

## Round 16.4 — active workflow Actions are SHA-pinned

**R-09's mutable-tag issue is implemented on this branch and was validated by CI (run 40108690774, six jobs green, at `7948e8f`).**
The four active workflows under `.github/workflows/` now pin all 36 external
Action references to reviewed 40-character commit SHAs, with the former version
tag retained as a comment. The "still the final check" sentence below is
history: that check passed, and the pins are unchanged since. The two local reusable-workflow references in
`publish-release.yml` remain `uses: ./.github/workflows/...` because they name
repository files rather than third-party Actions.

Only those 36 `uses:` reference lines changed. Workflow triggers, permissions,
release-gate conditions, signing environment, secrets wiring, build commands
and artifact handling were left unchanged. `tests/gaps.test.js` and
`tests/command_wiring.test.js` now scan the **active** workflow set and check
the complete reference count and reviewed SHA mapping; the CI run for this
commit was still the final syntax/integration check at the time of writing.

The former token-scope blocker was not bypassed by changing credentials: these
edits were accepted on the PR branch, and `main`, tags and releases were not
modified. The hardened files in `ci-fixes/` remain reference copies, not the
workflows GitHub executes.


## Fixed

### Android OAuth callback interception (R-05)

Native AniList login on Android accepts only a loopback redirect, the same
mechanism desktop uses, and a login started with a custom scheme is refused
before any pending state is written or an authorization URL exists.

One correction to the earlier wording ("the intent filter is removed"): the
built Android manifest still declares the `animehub://anilist` filter, because
`scripts/android_prepare.py` injects it into every generated manifest and
`tests/gaps.test.js` pins that it is added exactly once. `plugins.deep-link.mobile`
being `[]` only means the Tauri plugin does not add one. The filter is inert for
OAuth: nothing ever sends a redirect to that scheme, and an unsolicited
`animehub://anilist/callback` intent is parsed against the configured (loopback)
redirect and rejected. What makes R-05 fixed is the refusal, not the absence of
a filter — another app can still register the scheme, but there is no OAuth intent for it to receive.
Removing the dead filter is a manifest change that needs an Android build and
device check, so it is left as a residual.

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

All 36 external Action references in the four **active** workflows are now
pinned to the reviewed commit SHAs documented below and guarded by tests over
the active files. The `ci-fixes/` copies are retained as references; they are
not the executable workflow source. The separate proposal to assert the
**resolved** Gradle dependency graph in CI remains unapplied because that
extra step has not been exercised in this environment. `android_prepare.py`
continues to require each Gradle dependency pin exactly once.

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
* **WebRTC / ICE / STUN / TURN / UDP is NOT pinned.** The proxy carries TCP
  only and Android WebView cannot be told to force WebRTC through it — see
  [round 16.1](#1-webrtc-ice-stunturn-and-udp-not-covered-still-open). The
  Android init script removes the WebRTC API as defence in depth; that is not a
  guarantee and this document does not treat it as one.
* **Other apps on the device.** The listener is `127.0.0.1` only, so it is not
  reachable from the network, but another app on the same device could connect
  to it. Doing so gains that app nothing it could not do directly (the proxy
  only reaches public addresses and holds no credentials, and it never sees
  plaintext), and it cannot make the app's WebViews bypass the proxy.
* **A process crash** leaves the OS with no override once the process is gone;
  a new process starts unpinned and refuses to open a site until it installs
  the override again. That is the intended fail-closed path, not a gap.

## Round 16.4 — `start()` is all-or-nothing, and what that does not buy

### Same round: three defects fixed outside the Android proxy

| Finding | What was wrong | Now | Tests |
| --- | --- | --- | --- |
| iOS cookie import (GD-01) | `windows.rs::import_cookies()` replayed **every** live cookie in the store into the shared `WKWebView`, including cookies recorded for other sites, expired ones and cookies with no host; the `url` argument it was given was dropped. | Both import paths filter through `cookies_belonging_to_site()`: only cookies whose domain is the site or a parent of it, still live at `now`, reach the WebView. A foreign cookie is skipped, never an error. Android's migration already used the same rule and keeps it. | `the_shared_webview_import_only_restores_the_sites_own_cookies` (Rust). The iOS build itself is unverified here — it is a macOS-only path. |
| Resolver threads (GD-02) | `dns.rs::lookup_with_timeout()` spawned one detached thread per cache miss and forgot it at the timeout, so a burst of slow names could pile up unbounded OS threads, each holding a resolver call the platform will not cancel. | `lookup_with()` keeps a per-host in-flight entry that concurrent callers join, and resolves at most `MAX_LOOKUP_THREADS = 4` distinct hosts at once. A caller over the cap or past its deadline gets `DnsClass::Unknown` immediately instead of a queued thread. A timeout still only stops **waiting**: the platform call is not cancellable, a late answer never reaches the caller that timed out and is never cached, and `Unknown` is never cached. | `a_resolver_answer_is_classified_and_an_error_is_unknown`, `concurrent_callers_for_one_host_share_a_single_lookup`, `hung_hosts_cannot_exceed_the_lookup_budget`, `a_timed_out_lookup_never_classifies_its_late_answer`, `a_lookup_never_writes_the_answer_cache_itself` |
| Store temp file (GD-05) | `secure/store.rs::atomic_write_locked()` created its temp file with plain create+truncate and the process umask, so while it held the store's plaintext it could be group/world readable, and a same-directory squatter was silently truncated. | The temp is created with `create_new` and mode `0600` on Unix; the Windows ACL is unchanged (there is no chmod equivalent to tighten). A failed write or fsync removes the temp and leaves the existing target untouched; rename/fsync/cleanup are as before. | `the_store_file_is_private_to_its_owner`, `the_temp_file_is_created_exclusively_and_never_truncates`, `a_failed_write_leaves_the_existing_target_untouched` |

### Login ordering: bind before the browser is opened (desktop fixed port)

`anilist_login_start()` used to arm a pending state, return the authorization
URL and only *then* let the background task bind `127.0.0.1:<port>`. A local
process holding the fixed default port (17395) turned that into a late bind
failure: the user had already been sent to AniList, and the redirect — which
carries both `code` and `state` — landed in the process that held the port
while this app had nothing listening. There is no PKCE and no dynamic port in
this code, so a leaked `code` is only protected by the client secret at the
token endpoint.

`anilist::prepare_login()` now binds first and builds the URL and its state
afterwards, so a busy port is a plain refusal with no URL and no armed state
(`prepare_login_refuses_a_busy_loopback_port`,
`prepare_login_binds_the_return_path_before_building_the_url`). The residual is
availability, not secrecy: the port is still fixed, so a squatter can deny
desktop login — it now fails cleanly instead of losing the authorization.

`start()` used to build the proxy step by step: take a resolver generation,
create and bind the listener, publish the fields, create two pools, start the
accept thread. Every failure after the bind left something behind — most
importantly the resolver generation, which is budgeted (round 16.3), so a failed
start could consume part of a ceiling meant to survive for the whole process.
The failure could also be observed in an inconsistent order: `boundPort` was set
before the accept thread existed, so a start that threw could still report a
port.

What changed:

| Step | Now |
| --- | --- |
| Configuration | `Limits.validate()` runs first and is also enforced by `Limits`' `init` (so `copy()` cannot produce a bad one). Zero/negative timeouts, `maxConnections`, `backlog`, `headerBytes`/`bufferBytes` bounds and the resolver numbers (including an overflow-safe `2 * resolveThreads`) are rejected **before** any socket, thread or generation exists. |
| Listener | Created and bound; a failure closes the socket and returns the generation to the budget. |
| Pools / accept thread | Created inside one `try`; any failure resets every published field (`running`, `server`, `handlerPool`, `pumpPool`, `resolvePool`, `acceptThread`, `boundPort`), closes the listener, shuts the pools down and returns the generation. |
| "Is it started?" | `port` is only set immediately before the accept thread is started, and is the same value `start()` returns. A throw leaves `port == null`, so `SiteProxyController` cannot install a proxy that is not accepting. |

The phases are injectable (`StartHook` + `StartPhase`, default no-op) so each one
is covered by a test that makes the *real* code path fail at that point and then
checks the rollback and a clean retry — not by a mock of `start()`.

**What this does not buy.** A failed start is still a refusal, never a fallback:
`SiteProxyController` stops the half-built instance, marks the gate failed and
only the `setProxyOverride` callback may mark it ready, so nothing opens without
a confirmed proxy. And `SiteProxyGate` is terminal by design: one failed install
means the process refuses to open sites until it restarts. These tests say
nothing about a real device or emulator — the WebView install path is unchanged
and still unverified on hardware (checklist items 20–35).

## Round 16.3 — reusing resolver generations, and a cap that survives `stop()`

Round 16.2 added a spare generation but only ever submitted to the newest one,
and it cleared the generation list in `stop()`. Both were measured as broken:

* **An idle generation was never used.** With `resolveThreads = 1`,
  `resolveThreadCap = 2`, generation 1 wedged, generation 2 wedged, then
  generation 1's call returned: the next `CONNECT` was still refused with
  `RESOLVE_EXHAUSTED`, because the cap was spent and only generation 2 — still
  full — was ever tried. A usable thread sat idle while DNS was declared down.
* **A restart could spend the budget twice.** `stop()` shut the generations down
  and cleared the list. A call that ignores the interrupt keeps running, so
  `start()` then saw an empty list, created a fresh generation, and the object
  ended up with more resolver threads than its declared cap. The list was
  bookkeeping; the threads were real.

### What changed (all of it measured on the JVM, none of it on a device)

| Behaviour | Now |
| --- | --- |
| Where a lookup is submitted | The current generation first, then every older generation of the same lifecycle that is not shut down. Only when none can take it is another generation created, and at most one per call. |
| What the cap counts | `resolveThreads` per live generation **plus** `activeCount` of shut-down generations — the calls that are still running and cannot be reclaimed. It spans `stop()`/`start()` on the same object. |
| `stop()` | Shuts the generations down but **keeps tracking them**. Their threads still exist; forgetting them is what let a restart overspend. A generation that really terminated is dropped on the next accounting pass. |
| `start()` when the budget is spent | Throws. The listener is checked before it is bound, so a proxy that cannot resolve never looks ready; `SiteProxyController` maps it to `proxy-unavailable` and the site is not opened. |
| `Limits` | The cap must allow a **full** spare generation (`>= 2 × resolveThreads`); `4/5` — accepted in 16.2 and unable to form one — is now rejected at construction. Capacity is spent in whole generations, so a cap that is not a multiple of `resolveThreads` leaves the remainder unused rather than creating a half generation. Defaults stay `4/8`. |

One more defect surfaced while running this suite repeatedly on JDK 25:
`stop()` closed the live sockets from a `live.toList()` snapshot, and that
iteration throws `NoSuchElementException` when a handler removes its own socket
between `hasNext()` and `next()` (observed as an intermittent failure of
`a_cleartext_request_through_the_proxy_is_refused`). A `stop()` that throws there
leaves descriptors open, so the sweep now iterates the set directly and skips a
vanished entry. It is a pre-existing bug, not one introduced here.

Tests that fail without these changes (each mutation was run and reverted):
`an_older_generation_is_reused_once_its_lookup_returns` (only-current
submission → 502 instead of 200), `a_restart_cannot_spend_the_resolver_budget_twice`
(clearing the list in `stop()` → the third `start()` succeeds),
`the_cap_must_leave_room_for_a_full_spare_generation`,
`a_cap_that_is_not_a_whole_number_of_generations_leaves_the_remainder_unused`.

### What is still not fixed

* **The platform call is still uncancellable.** Everything above bounds threads
  and reuses what is reusable; none of it can make `InetAddress.getAllByName`
  return. If every thread the budget allows is owned by a call that never
  returns, DNS stays refused until the process restarts. Reuse makes that far
  less likely to be permanent — a generation becomes usable again the moment one
  of its calls returns — but it is not recovery, and the docs do not call it
  that.
* **The refusal is per proxy object.** The budget is enforced by the object that
  owns the generations. The app installs one override per process and the gate
  permits a single install, so one object is the normal case; a second object
  would come from an install attempt that failed first.
* **A failed `start()` latches the gate.** `SiteProxyGate` refuses after a
  failed install and a later success cannot flip it, so a budget-spent start
  means the process refuses to open sites until it restarts. That is the same
  fail-closed path as a bind failure and is stated here rather than papered
  over.
* **Nothing here changes the DNS-rebinding verification status** — still OPEN
  until the device capture in checklist items 1–19 exists — or the loopback
  reachability residual, or the Action SHA-pinning blocker (R-09).

## Round 16.2 — resolver generations, and two claims corrected

### 1. The `start()` comment claimed app isolation from a loopback bind

The KDoc on `DnsPinnedProxy.start()` said the `127.0.0.1` bind meant the proxy
was not "reachable … from another app on the device". That is wrong, and it
contradicted both §2 below and device checklist §5c. Binding loopback is an
**egress** control — no Wi-Fi/hotspot/USB-tether/Docker interface can reach the
port — and not an isolation boundary between apps: Android loopback is shared,
and any app that finds the ephemeral port can send `CONNECT`. The comment now
says exactly that, points at this section, and `ProxyConfig` still offers no
credential mechanism, so the listener stays unauthenticated. **The residual is
open; nothing about it was "solved" by editing the comment.**

### 2. Resolver threads: bounded, and what the old test did not measure

The round-16.1 fake resolver waited on a `CountDownLatch`, so
`future.cancel(true)` released it. That models a *slow* lookup, not a wedged
one: it let the suite report "at most N threads" and "no 6× timeout wait" while
the real, uninterruptible case was untested. Round 16.2 adds a resolver that
swallows interrupts (`UninterruptibleResolver`) and measures what a genuinely
stuck platform call does. Measured on the JVM (not on a device):

| Scenario | Behaviour |
| --- | --- |
| One lookup never returns | `CONNECT` is refused `502` after `resolveMs`; the handler slot is released; the lookup keeps its thread. |
| Every thread of the current generation is stuck | The next `CONNECT` **rotates to one spare generation** (4 more threads, 8 total) and is resolved there. No restart is needed for this case. |
| The spare generation is stuck too | `Refusal.RESOLVE_EXHAUSTED` (`502`) immediately, for **every** host, because the shortage is in this process, not in that host's answer — but only once **every** generation is full: from round 16.3 an older generation with a free thread is reused first (see [round 16.3](#round-163--reusing-resolver-generations-and-a-cap-that-survives-stop)). Nothing is queued and nothing is dialled. |
| A stuck lookup finally returns | Capacity comes back by itself, the next `CONNECT` resolves again, and no failure was cached. |
| The lookups never return | DNS stays refused until the proxy/process is restarted. **This is the residual.** |
| `stop()` while lookups are stuck | Every generation is shut down and the listener closes, but a call that ignores the interrupt keeps its thread until the platform returns — `stop()` cannot end it. |

Thread count is bounded by `resolveThreadCap` (8 with the defaults) no matter
how many connections arrive; a fifth concurrent lookup is refused rather than
queued, and an abandoned generation's answers are dropped unread (a late
answer is never classified — `a_late_answer_after_a_timeout_is_never_classified_or_reused`).

### 3. Resolver generations: what "recoverability" this actually buys

Precisely this much: **a wedge no longer becomes permanent on the first four
stuck lookups.** A captive portal or a VPN switch that strands a whole
generation and then heals leaves the app working, where before it was DNS-dead
until relaunch. It is **not** automatic recovery in general — one spare
generation exists, the total is capped, and a platform resolver that never
returns still wedges the process, now at 8 threads instead of 4. The docs, the
naming (`mayCreateResolverGeneration`, `ResolverExhausted`) and the checklist
say that; none of them call it recycling, deduplication or self-healing.

Rejected alternatives, for the record:

* **A hand-written DNS client over UDP/TCP** would give real cancellation and a
  real timeout, but it bypasses the system resolver — Private DNS/DoT, VPN
  split-DNS, captive-portal handling and the user's configured servers — sends
  queries in cleartext to a server of our choosing, and puts DNS wire-format
  parsing (compression pointers, EDNS, truncation/TCP fallback) inside the
  security boundary. A silent parsing bug there would hand the pin the *wrong*
  address, which is worse than a refusal.
* **`android.net.DnsResolver.rawQuery` + `CancellationSignal`** (API 29+) is
  cancellable, but it still needs wire-format queries/answers, breaks this
  file's "pure `java.net`, so CI runs the real decision path" property, and its
  cancellation semantics cannot be verified here without a device.
* **Unbounded threads** (one per lookup) would remove the ceiling by turning a
  DNS hang into thread growth. That is the leak this design exists to prevent.

### 4. MSRV documentation drift, corrected

`docs/android-dns-pinning-device-checklist.md` still said `1.87` after the
manifests moved to `1.89`; it now says `1.89` and a JS test pins the checklist
to the manifest value so the next bump cannot leave it behind. The CI step was
re-read as well: it extracts `rust-version` from `src-tauri/Cargo.toml`,
installs exactly that toolchain, prints its `rustc --version`, runs
`cargo +<value> check --locked --all-targets`, and annotates the failure text.
Its green run is the evidence; the number in the docs is only a copy of what
that gate measures.

### 5. Claims re-checked in this round

* **HTTP(S)/WSS pinning is reported separately from WebRTC/ICE/STUN/TURN/UDP.**
  The pin covers TCP `CONNECT`. WebRTC/UDP is not pinned and is not claimed to
  be; the init script's API removal is defence in depth only.
* **DNS rebinding stays `OPEN` for verification purposes.** The design pins the
  connection to the classified address, but "verified" requires the device
  capture in checklist items 1–19 on at least two WebView providers. A JVM
  suite, a green CI run and a source review are not that.
* **Another app reaching the loopback port is not a solved risk.** The comment
  was fixed; the exposure is unchanged and stays open (§2 above).
* **Action SHA pinning is still blocked.** The active workflows reference
  mutable tags; the SHA-pinned equivalents exist only in `ci-fixes/` and the
  token lacks `workflow` scope to land them (R-09). A green
  `Dependency and secret audit` job says nothing about `uses:` pinning.

## Round 16.1 — what the proxy does NOT cover

Round 16 pinned one thing: the TCP connection a site WebView makes for an
HTTPS/WSS request. This section is the honest boundary of that statement, plus
the three problems an independent review found in the implementation.

### 1. WebRTC, ICE, STUN/TURN and UDP: NOT covered, still open

**The proxy carries TCP only.** An HTTP `CONNECT` proxy cannot carry UDP, and
this is not an implementation gap — it is what the protocol is. The relevant
standards and platform behaviour, checked rather than assumed:

* RFC 8828 §5.1, principle 4: *"By default, WebRTC traffic should not be sent
  through proxy servers … However, it should be possible to force WebRTC to send
  its traffic through a configured proxy if desired."* Mode 4 ("Force proxy") is
  the opt-in: media goes through the proxy, and *"if the proxy does not support
  UDP (as is the case for all HTTP and most SOCKS proxies) … the use of UDP will
  be disabled, and TCP will be used to send and receive media through the
  proxy."*
* Chromium exposes that opt-in as the `WebRtcIPHandling` enterprise policy /
  `disable_non_proxied_udp` preference. Even that is not a clean guarantee:
  Chromium's own tracker records that with `disable_non_proxied_udp` and no
  proxy configured it still permits **TCP** TURN, and that its Mode-4 behaviour
  falls back to "a TCP-only version of Mode 3" (Chromium issue 41345813,
  closed as working-as-intended).
* **Android WebView gives an app no way to set it.** `ProxyController` sets a
  proxy; there is no WebView API for the WebRTC IP-handling policy, no
  `chrome://flags`, no `--webrtc-ip-handling-policy` switch, and no per-app
  preference file the app may write.

So: **a page in a site WebView can create traffic that never reaches the
pinning proxy.** ICE/STUN/TURN datagrams go out over the device's own network
stack, from the device's own addresses, with no DNS answer of ours and no socket
of ours involved. Nothing in `SiteWebViewSettings` changes that (it has no
WebRTC field), and the document-start script runs *inside the page*, which is
not where the network is decided.

**What was done about it (defence in depth, explicitly not the guarantee).**
`src-tauri/src/web/webrtc_guard.js` is appended to the Android init script: it
replaces `RTCPeerConnection` and `webkitRTCPeerConnection` in every frame with a
constructor that throws `NotSupportedError`, using a non-configurable,
non-writable property, and keeps no reference to the platform constructor.
Tests execute the shipped file (not a paraphrase) against a window that has a
platform constructor, and assert that a page cannot reassign or delete the
override, that the native constructor is never invoked, and that the desktop
script does not contain the override at all. **A page-level script is not a
security boundary**: the document-start hook can be missed by a provider without
`DOCUMENT_START_SCRIPT` (the fallback `onPageStarted` can lose the race with an
inline script), and a future engine could expose another path. Anyone reading
this section should treat WebRTC as *uncovered* and the guard as reducing the
chance, not closing it.

**What would actually close it, and why it is not in this PR.** A
`VpnService` that routes the app's traffic through a userspace filter (or
refuses non-TCP) is the Android mechanism that covers UDP by construction — it
is also a user-consent-gated VPN session with a persistent notification, it
interacts with the user's own VPN, and it is a product decision, not a bug fix.
The alternative, a per-connection firewall for the app UID, needs root/`netd`
access an app does not have. Until one of those ships, the honest statement is:
**Android site views are IP-pinned for HTTP(S)/WSS and not for WebRTC/UDP.**
The device checklist turns this into a measurement (packet capture while a page
creates a peer connection) instead of an argument.

### 2. The loopback listener is reachable by other apps: open, bounded, documented

`DnsPinnedProxy` binds `127.0.0.1` only, which stops *network* access. It does
**not** make the port private to this app: Android's loopback is shared between
apps, and the platform documentation says so directly — *"We have seen some
applications use localhost network ports for handling sensitive IPC. We
discourage this approach since these interfaces are accessible by other
applications on the device. Instead, you should use an Android IPC mechanism
where authentication is possible such as with a Service."* (Android security
guidance, quoted in CERT DRD23-J; the same property is what the "Bridges to
Self" research exploits across browsers and native apps).

What that means here, precisely:

| Question | Answer |
| --- | --- |
| Can another app reach the port? | **Yes**, if it scans or guesses it. The port is ephemeral and per-process, which is a speed bump, not access control. |
| Can it use the proxy as a general egress path? | **Yes** — the proxy only refuses non-public destinations, so it is not an open relay into the device or LAN, but it *is* a working public HTTP(S) egress. |
| Can it read this app's traffic or credentials? | **No.** It cannot see other connections' bytes (each tunnel is its own socket pair), TLS is end-to-end so no plaintext exists, and the proxy holds no cookies or tokens. |
| Can it make the site WebView bypass the pin? | **No.** Nothing another app does can remove the `ProxyController` override; the override is process-wide and never cleared. |
| Can it deny service to the site WebView? | **Partially.** It can occupy handler slots. Measured bounds: 16 connections (`maxConnections`), a 5 s handshake deadline per connection, no queue at all (`SynchronousQueue`), and a flood beyond the ceiling is refused with `503` immediately (`an_idle_handshake_flood_is_bounded_by_the_handshake_timeout`, `the_connection_ceiling_refuses_instead_of_queueing`). So the worst case is a bounded, self-healing refusal window, not a deadlock. |

**Authentication is not available.** `ProxyConfig.Builder` has no credentials
API, and the WebView offers no hook that would attach a `Proxy-Authorization`
header to its own `CONNECT` for a fixed proxy. A `407`-challenge scheme would
therefore have to be answered by engine-supplied credentials that do not exist;
a mechanism that *probably* works on one provider and breaks site loading on
another is worse than a documented bound. So the listener stays unauthenticated
and the residual is stated, not papered over. The mitigations that are real:
loopback-only bind, ephemeral port, public destinations only (so it is not a
pivot into the device or the user's LAN — see
`another_app_cannot_use_the_proxy_to_reach_local_or_private_services`),
bounded concurrency and handshake deadlines, and no logging of hosts or URLs, so
another app's use of the port leaves no data behind for this app to leak.

### 3. Resolver timeout and socket lifecycle (round 16.1; the pool was revised in 16.2 and 16.3)

`InetAddress.getAllByName` has **no timeout and no cancellation** — with a
captive portal or a blackholed DNS server a single `CONNECT` could hold a
handler thread, and 16 of those would wedge every site request. What changed:

* Resolution runs on a bounded pool with a bounded wait
  (`Limits.resolveMs = 10 s`). A timed-out future is cancelled best effort;
  because the platform call cannot be interrupted, the abandoned lookup keeps
  its pool thread — never one thread per connection. A further `CONNECT` while
  a generation is full does not queue. Round 16.2 adds a **spare generation**
  and a hard ceiling on total resolver threads
  (`Limits.resolveThreads = 4` per generation, `Limits.resolveThreadCap = 8`):
  a wedged generation is replaced once, and past the ceiling the answer is
  `Refusal.RESOLVE_EXHAUSTED` — still a `502`, still fail-closed. This is
  capacity, not recovery: see [round 16.2 §3](#3-resolver-generations-what-recoverability-this-actually-buys).
  Round 16.3 corrects two defects of that design — a lookup now also goes to an
  older generation that became usable again, and the ceiling counts the threads
  of a previous `stop()`/`start()` cycle instead of being reset by `stop()` (see
  [round 16.3](#round-163--reusing-resolver-generations-and-a-cap-that-survives-stop)).
* A timeout, a resolver exception and an empty answer are all `502` refusals and
  **no address is classified from them**; nothing is cached in either
  direction, so a failure cannot become a public verdict
  (`a_failed_resolution_is_not_remembered_as_a_verdict`).
* `stop()` now closes every socket the proxy owns (accepted clients and upstream
  connections) **after** `shutdownNow()`. The interrupt does not end a thread
  blocked in `InputStream.read()`; closing the socket does. Clients observed EOF
  in `stop_closes_live_handshakes_and_tunnels`, which also covers a client
  parked mid-handshake, and the upstream side is torn down with it.

### 4. Declared MSRV: 1.77 was not true, and is now 1.89 with a CI gate

`src-tauri/Cargo.toml` declared `rust-version = "1.77"` while the locked graph
already needed 1.77.2 (tauri 2.11.6), 1.85 (`crypto-common` 0.2.2, `sha2` 0.11,
`hmac` 0.13, `cmov` 0.5.4 — all `edition = "2024"`), 1.87 (`ctutils` 0.4.5,
after the RUSTSEC-2026-0385 fix), 1.88 (`darling` 0.24.1, `icu_*` 2.3,
`keyring` 4.2, `plist` 1.10.1, `serde_with` 3.23, `time` 0.3.55) and finally
**1.89** (`aes` 0.9.3). Cargo refuses to build a crate whose declared
`rust-version` is below a dependency's, so 1.77 was never a working promise.

Both manifests now declare **1.89**, the README says so, and a CI step reads the
value from `Cargo.toml` and runs `cargo +<declared> check --locked
--all-targets`. This is how 1.87 was rejected: the first version of this change
guessed `1.87` from the crates the review named, the new gate answered `error:
rustc 1.87.0 is not supported by the following packages: … aes@0.9.3 requires
rustc 1.89`, and the declared value was corrected to what the graph actually
needs instead of the gate being weakened. The security fix was not reverted and
no advisory is ignored: `ctutils` stays at 0.4.5.

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

### Actions pinning (R-09) — state at Round 7, superseded by Round 16.4

At the time of Round 7, the active workflows still used mutable tags and
`workflow_dispatch` was refused for the available token
(`HTTP 403: Resource not accessible by integration`), so CI could only be
triggered by a pull request. The SHA-pinning status changed in Round 16.4; see
[Actions pinning — verified SHA table](#actions-pinning--verified-sha-table-applied-in-round-164).

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
matrix that has to run before any of it can be shipped. That network layer was
**not** part of this PR at the time of writing — *superseded*: round 16 built it
for Android (`SiteProxyController` + `DnsPinnedProxy`, `PROXY_OVERRIDE`) and
round 17 built it for desktop (`web/pinned_proxy.rs`, installed through wry's
`proxy_url` on Linux, `additional_browser_args` on Windows and `macos-proxy` on
macOS). Neither half is device-verified; the table at the
top of this document is the current state. **R-01 (b) stays open.** No comment,
test or doc change closes it.

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

### Actions pinning — verified SHA table (applied in Round 16.4)

This table records the reviewed references that are now used by the active
workflows. Earlier rounds below describe the state at those points in time;
they are historical and are superseded by the Round 16.4 status at the top.

| Action | Verified commit SHA | former tag |
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

There are 36 external `uses:` references: 19 in `ci.yml`, 7 in
`release-android.yml`, 6 in `release-desktop.yml`, and 4 in
`publish-release.yml`. The latter file's two local reusable-workflow
references are deliberately left as relative paths. The changes replace only
the action reference on each of the 36 lines; all workflow logic stays the same.

Pinning `dtolnay/rust-toolchain` fixes the installer **Action code** at a
commit, not the Rust toolchain installed by its `stable` input. That is not a
claim of bit-for-bit reproducible Rust toolchains.

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
