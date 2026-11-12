// SPDX-License-Identifier: MIT
//
// Page-level egress API guard. This is defence in depth, NOT a network
// boundary and NOT proof that UDP or QUIC is confined to the proxy.
//
// WHY WEBRTC IS REMOVED: the connection-pinning proxy (`DnsPinnedProxy.kt` on
// Android, `pinned_proxy.rs` on desktop) is an HTTP `CONNECT` proxy, and
// `CONNECT` carries TCP only. Chromium's WebRTC stack gathers ICE candidates
// over UDP and, per RFC 8828 section 5.1 principle 4, does NOT send that
// traffic through a configured HTTP proxy by default. A page that opens an
// `RTCPeerConnection` can therefore create traffic that never reaches the
// proxy and is never pinned to a classified address.
//
// WHY WEBTRANSPORT IS REMOVED WHERE PRESENT: WebTransport runs over HTTP/3,
// which is QUIC over UDP. A `CONNECT` tunnel cannot carry it. Removing the
// window constructor closes the direct page API on the main thread. It does
// NOT make QUIC impossible: `WebTransport` is also exposed inside dedicated
// and shared workers, and a document-start injection script does not run in
// worker realms. It also cannot control QUIC that the browser's own HTTP
// stack chooses to use for ordinary requests. Those residuals need an OS or
// network-level control and are not claimed as fixed here.
//
// WHAT THIS IS NOT: a sandbox. A page runs in the same realm as this script.
// If the document-start hook is late or absent, an inline script can capture
// the native constructors before this file runs. A page script cannot stop
// that after the fact. Android refuses to open a site when
// `DOCUMENT_START_SCRIPT` is unsupported; desktop registration cannot be
// verified from the page. A `VpnService`-based, user-consented network filter
// would be the real enforcement for "no traffic outside the pinned path";
// until that exists, sites are pinned for HTTP(S)/WSS, not for ICE, STUN,
// TURN, raw UDP or QUIC. The evidence and remaining gaps are recorded in
// `docs/security-residual-risks.md` and the PR that introduced this file.
//
// The replacements are deliberately non-configurable and non-writable: a page
// that tries to `delete` or reassign them fails instead of restoring the
// platform constructor. This protects the override itself; it cannot repair a
// missed injection.
//
// MEDIA IMPACT: this file does not touch `MediaSource`, `SourceBuffer`,
// `HTMLMediaElement`, `URL.createObjectURL` or the PiP controller. Ordinary
// HTML5, MSE, HLS and DASH playback is unaffected. Playback that actually
// depends on WebRTC (for example a low-latency RTC live stream) will receive
// `NotSupportedError`; WebTransport-based low-latency playback loses its
// constructor in the main thread. Those are deliberate security trade-offs.
(function () {
  var refuse = function (feature) {
    var err = new Error(
      feature + " is disabled in AnimeHub: it would bypass the pinned network path",
    );
    err.name = "NotSupportedError";
    err.code = 9;
    throw err;
  };

  var blocked = function () {
    refuse("WebRTC");
  };
  blocked.prototype = {};

  var install = function (name, replacement) {
    try {
      Object.defineProperty(window, name, {
        value: replacement,
        configurable: false,
        writable: false,
        enumerable: false,
      });
      return;
    } catch (e) {
      // An old provider can refuse redefinition. Assignment is the next best
      // thing while the property is writable. If that also fails, the native
      // API may remain reachable; that is an open residual, not silently
      // treated as success.
      try {
        window[name] = replacement;
      } catch (e2) {
        // Nothing else can be done from a page script.
      }
    }
  };

  var names = ["RTCPeerConnection", "webkitRTCPeerConnection"];
  for (var i = 0; i < names.length; i++) {
    install(names[i], blocked);
  }

  // Feature detection must stay honest: only replace WebTransport when the
  // provider exposes it. Defining it on an older WebView would make a site
  // believe a transport exists and then fail at construction time.
  if (typeof window.WebTransport === "function") {
    var blockedTransport = function () {
      refuse("WebTransport");
    };
    blockedTransport.prototype = {};
    install("WebTransport", blockedTransport);
  }
})();
