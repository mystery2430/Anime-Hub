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
address for the connection (local CONNECT proxy or platform-level filter), not
another WebView callback. A resolver outage is deliberately fail-open so an
offline DNS server does not brick the app.

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
