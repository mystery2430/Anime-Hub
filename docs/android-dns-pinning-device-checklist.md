# Android: connection-pinning proxy — device checklist

**Status: NOT YET RUN.** No physical device or emulator was available while
this was written. Everything below is a script for a human, not a report of
observed behaviour. Until it passes, DNS rebinding (R-01 b) stays **OPEN** in
`docs/audit-item-status.md`, however green the JVM and CI suites are.

What *is* covered without a device (so this list can start where automation
stops):

* `DnsPinnedProxyTest` — real sockets: CONNECT parsing (malformed header,
  bad authority, bad port, ambiguous IPv6), one resolution per CONNECT,
  mixed/empty/throwing answers refused, `127.0.0.1` / `10/8` / `172.16/12` /
  `192.168/16` / link-local / `::1` / ULA / IPv4-mapped / alternative IPv4
  spellings refused, connect by `InetAddress`+port with no second lookup,
  `socket.inetAddress` verification, bounded header/connect timeouts, the
  concurrency ceiling, clean shutdown, early client disconnect, cleartext
  refused.
* `SiteProxyGateTest` — the install state machine: wait before first open,
  unsupported/failed/timeout all map to `proxy-unavailable`, no path returns
  "open unpinned".
* CI "Android compile (aarch64)" — the real sources compile against the real
  androidx.webkit API; that proves signatures, not WebView behaviour.

What no JVM test can prove: **what the Chromium engine actually does with a
proxy override.** That is what this checklist is for.

---

## 0. Preparation

```bash
# two provider configurations, because behaviour differs by version:
adb shell dumpsys webviewupdate | head -20          # current provider + version
# 1) up-to-date Android System WebView / Chrome
# 2) a deliberately older provider (the downgrade path)
```

Build/toolchain check that belongs with this matrix: the declared MSRV is
`1.89` (`src-tauri/Cargo.toml`, same value in `android-plugin/Cargo.toml`); CI
reads that value out of the manifest, installs exactly that toolchain and runs
`cargo +<value> check --locked --all-targets`, so a local `rustc` older than
that is a support question, not a bug report.

Tools:

```bash
# capture all app traffic without root on the device: use a Wi-Fi hotspot on a
# laptop running tcpdump, or `adb shell` with tcpdump on a rooted image.
sudo tcpdump -ni any 'tcp port 443 or udp port 443' -w capture.pcap
```

For every item below record: provider name + version, Android version, app
build (commit SHA), PASS/FAIL, and the in-app debug log excerpt. A FAIL on
items 1–5 means the feature does not ship; items 6–14 are regressions to fix.

The in-app diagnostics log has an area `proxy` with phases `install` and
`ready`; the refusal surfaces as the user-facing message in
`android_bridge.rs` (`PROXY_UNAVAILABLE_MSG`). If a check fails, the log should
already say which phase stopped.

---

## 1. Feature support, and safe refusal when it is missing

1. On the up-to-date provider, open a site. **Expected:** the site opens and
   the log shows `proxy/install` then `proxy/ready` before the first page
   render.
2. Force the unsupported path. Simplest reliable method: build a debug variant
   with `isFeatureSupported` short-circuited to `false` (or point the emulator
   at a provider without `PROXY_OVERRIDE`, e.g. an old System WebView).
   **Expected:** the site does **not** open, no WebView content flashes, and the
   user sees the `PROXY_UNAVAILABLE_MSG` text ("… güvenli biçimde
   sabitlenemedi …"). **Not** a spinner, **not** the site in degraded mode.
3. Same on the downgraded provider (uninstall WebView updates, retry).
   **Expected:** identical refusal, no crash, no ANR, and the launcher itself
   keeps working.
4. Kill the app between install attempts (open a site, force-stop during
   install, relaunch, open a site). **Expected:** a new install attempt runs and
   either succeeds or refuses; it never opens a site unpinned.
5. Try to open a site twice quickly (double-tap a card). **Expected:** one
   install, one view; the second open either waits for the same install or is
   coalesced — no second proxy instance, no second `setProxyOverride`.

## 2. The proxy is applied, and the launcher/IPC still work

6. After a site has been opened once, go back to the launcher. **Expected:**
   the launcher renders fully — cover images, transitions, and any launcher
   action that talks to Rust over Tauri IPC (settings read/write, blocklist
   push, diagnostics export).
   *Why this is a real risk:* `ProxyController` is process-wide, so the
   launcher WebView is governed too. wry 0.55.1 answers `https://wry.assets/`
   and the IPC path inside `shouldInterceptRequest`, so they should never reach
   the proxy — this check is what proves that on a real engine. If the launcher
   does break here, it will break **closed** (refused request), which is the
   right direction, but it is still a release blocker.
7. On `chrome://inspect`, open the site WebView's devtools: every document and
   sub-resource should show a remote address that is a **public** IP, and never
   the site's hostname resolved locally by the device.
8. With the proxy installed, open a site that uses `localhost`/`.local` and one
   whose page fetches `https://127.0.0.1/`. **Expected:** the navigation policy
   refuses it, and even if it did not, the proxy refuses the `CONNECT` (403)
   and the page sees only the empty 403 (no body, no error text).

## 3. Rebinding cannot leave the checked IP

9. Use a **local** rebinding test host (no public rebinding service, no real
   internet dependency): run a DNS server on the laptop that answers a public
   address on the first query and `127.0.0.1` / `10.0.0.1` on every later
   query, point the device's resolver at it, and open `https://rebind.test`.
   **Expected, in order:**
   * the first `CONNECT` resolves once, sees a public address, connects to that
     address, and the TLS handshake fails (the test host has no valid cert for
     `rebind.test`) — a cert error shown in the WebView is the **correct**
     outcome here: the proxy must not swallow or bypass it;
   * a subsequent attempt (now the name answers `127.0.0.1`) is **refused at
     CONNECT**; capture shows no TCP connection to `127.0.0.1` for that
     request, and none to the public address either;
   * repeat the same test with an answer that is *mixed* (one public + one
     private address in the same response): **refused**, no connection at all.
10. Re-run 9 and watch `tcpdump`: the only outbound destination for that
    hostname must be the address that was classified and logged-then-dropped
    (the app logs no DNS results; use the capture, not the log). If the capture
    shows two different public addresses for one `CONNECT`, the pin is broken —
    FAIL and report.
11. Repeat 9 with a host that answers **empty** and with one that makes the
    resolver throw (kill the DNS server mid-request). **Expected:** `502` to
    the WebView, no direct fallback, no crash.

## 4. No direct egress when the proxy is down or failing

12. Open a site, then make the proxy useless while the WebView is alive
    (kill the app's process is *not* this test: instead stop only the proxy —
    e.g. a debug hook, or exhaust the connection ceiling by opening many
    parallel streams). **Expected:** requests fail; there is **no** packet to
    the site over a direct connection. Checking this in `tcpdump` is the whole
    point: "the page shows an error" is not proof.
13. Force a re-install failure (revoke the config path in a debug build so
    `setProxyOverride` throws) while a site view exists. **Expected:** new
    navigations fail (or the site is closed with the refusal message); the
    existing session is never silently unpinned.
14. Memory pressure: open several sites, then send the app to the background
    and let the system kill it. **Expected:** on relaunch, no site opens
    without a fresh successful install (fail closed, not stale state).

## 5. Protocol coverage

15. **Redirects.** Use a short redirect chain (test host: 302 → another host →
    200). **Expected:** a new `CONNECT` per hop, each classified separately;
    a hop that resolves to a private address is refused while the earlier hops
    succeeded. Do this with the DNS flip script from item 9 flipping between
    hops.
16. **Sub-resources and iframes.** A page with images/CSS/scripts from three
    hosts and an iframe from a fourth. **Expected:** a `CONNECT` per host in
    the capture; private-address hosts are refused individually and the page
    degrades gracefully (no white screen, no crash).
17. **Video.** Play a real video on the two built-in sites, including seeking.
    **Expected:** playback works through the tunnel (range requests survive,
    bytes are not buffered into memory), seeking does not stall, no ANR, no
    "proxy timeout" under sustained streaming.
18. **WSS/WebSocket.** A site with a live chat or a websocket-driven player.
    **Expected:** the upgrade goes through `CONNECT` and works; a websocket to
    a private address is refused.
19. **HTTP/3 / QUIC.** During item 17, confirm the capture shows **no UDP 443**
    from the app. Expected: none, because a proxied request is TCP. If QUIC
    appears, the engine bypassed the proxy — treat as FAIL and investigate.
20. **Login and cookies.** Log in on a site, navigate within it, restart the
    app, confirm the session survives. **Expected:** logins work and sessions
    persist exactly as before the proxy (the tunnel does not touch cookies or
    the profile store).
21. **PiP and back.** Enter PiP from a playing video, return, press back
    through history to the launcher. **Expected:** no proxy-related error, no
    dropped `CONNECT` when the activity is paused/resumed, playback continues.
22. **Background/foreground and site close.** Send the app to the background
    for several minutes with a site open, bring it back; then close the site
    and open it again. **Expected:** the override is still installed (it is
    process-wide and never cleared), the connection behaves the same, and no
    second proxy instance is started.

## 5b. Traffic the proxy does NOT carry (WebRTC / UDP / QUIC)

The proxy is `CONNECT`-only, so **UDP never passes through it**. Two separate
claims are tested here, and only the first one is a guarantee:

* `HTTP(S)/WSS` really goes through the tunnel and is pinned (sections 3, 4, 5);
* `WebRTC` is **not** pinned and can only be removed at the API level, which is
  defence in depth.

26. **Baseline capture.** With a site open and a video playing, run
    `tcpdump -ni any 'udp port 443 or udp port 19302 or udp port 3478' -w base.pcap`
    for 60 s. Expect: no packets from the app's UID. (Chromium does not use QUIC
    for a proxied request, so no UDP 443 either.)
27. **WebRTC attempt.** Load a test page that runs
    `new RTCPeerConnection({iceServers:[{urls:'stun:stun.example:3478'}]})` and
    `createOffer()` against a STUN server you control on the capture host.
    Expected in the app: the constructor throws `NotSupportedError` and the
    page sees no candidates; expected in the capture: **no UDP from the app**.
28. **Prove the guard is what stops it, not the engine.** On a debug build,
    build once with `webrtc_guard: false` (commands.rs) and repeat 27. Expected:
    STUN datagrams **do** leave from the device's own address, bypassing the
    proxy entirely. This is the residual that is documented as open; if this
    step produces no datagrams either, the test is inconclusive (check the
    capture filter and that the page really created a peer connection).
29. Repeat 27 on a provider without `DOCUMENT_START_SCRIPT` (older System
    WebView) — the fallback `onPageStarted` can lose the race with an inline
    page script. If candidates appear there, that provider is **not** covered by
    the API guard; record it.

## 5c. The loopback listener is reachable from another app (expected)

30. From a second app (or `adb shell` on a rooted/debuggable image), scan for
    the proxy port and connect with
    `printf 'CONNECT example.com:443 HTTP/1.1\r\n\r\n' | nc 127.0.0.1 <port>`.
    Expected: **it works** — the port is not private to the app (documented; see
    `docs/security-residual-risks.md` round 16.1 §2). Record it as observed
    behaviour, not as a bug.
31. From the same second app, ask the proxy for a local target
    (`CONNECT 127.0.0.1:22`, `CONNECT 192.168.1.1:443`) and for a name that
    resolves to a private address. Expected: `403` every time and no socket
    opened — the listener cannot be used as a pivot into the device or the LAN.
32. Flood it: open ~50 sockets that send nothing, then use the site in the app.
    Expected: refused (`503`) while the flood holds the slots, and recovery
    within about 5 s (the handshake deadline) without restarting the app.

## 5d. Resolver behaviour on a real device

33. Point the device's DNS at a blackholed address (or enable airplane-mode DNS
    behaviour on a captive portal) and open a site. Expected: requests fail
    within ~10 s (`502`), the app stays responsive, and repeat attempts fail
    just as fast rather than queueing. Check `dumpsys` / thread dump: at most
    `resolveThreads` threads per generation and never more than the cap of 8
    (`AnimeHubProxyResolve`), not one per attempt.
34. Restore DNS and retry. Expected: the site opens — nothing about the failed
    lookup is remembered. If four lookups were still stuck when DNS came back,
    the first retry should be served by the **spare generation** without
    restarting the app (JVM behaviour; on-device this is what item 34 confirms
    or refutes).
35. Leave DNS blackholed long enough to wedge both generations (8 stuck
    lookups: keep opening sites for a few minutes), then restore DNS. Expected:
    requests keep being refused (`502`) immediately, because the threads are
    gone and `stop()` cannot reclaim a call that ignores the interrupt. The
    documented recovery is to restart the app, which is a **known availability
    residual, not a bug you are hunting**. Record it as observed behaviour, and
    capture a thread dump showing that the resolver thread count never exceeds
    8 and does not grow with each attempt.

## 6. Isolation, unchanged by this work (regression check)

23. Two sites in two windows: log into both, confirm each keeps its own
    cookies and `localStorage` (per-site profile via `MULTI_PROFILE`), and
    that the launcher's default profile is untouched.
24. Confirm the site WebView still has no Tauri capability and no `ipc`
    object: evaluating `window.__TAURI__`/`ipc` in the site devtools must fail
    the same way it did before this round.
25. Confirm a refused open leaves no zombie: after item 2 or 13, the task
    manager/`dumpsys activity` shows no leftover site window, and the next open
    attempt works once the provider supports the feature again.

---

## What would count as proof, and what would not

* **Proof:** capture showing the only outbound TCP connection for a hostname is
  to the address that was classified, with a second resolution answering a
  private address producing **no** connection at all; plus the refusal message
  on unsupported providers; plus items 6, 7 and 17–21 passing. **WebRTC is not
  part of that proof**: item 28 shows UDP leaving the device when the API guard
  is off, which is why the honest claim is "HTTP(S)/WSS is pinned", never "all
  network traffic is". Item 30 (another app reaching the port) is expected
  behaviour and must be recorded, not "passed".
* **Not proof:** the site loads; the unit tests pass; the JVM socket tests pass
  (they pin the *logic*, not the engine); the source contains the right calls.

Report each item as PASS/FAIL with the capture file and log excerpt. This
document moves R-01 (b) from "mitigated by design" to "verified" only when
items 1–19 have run on at least two provider versions. Even then, the WebRTC
residual (items 26–29) and the shared-loopback residual (30–32) stay open;
they need a `VpnService` filter and proxy authentication respectively, neither
of which exists.
