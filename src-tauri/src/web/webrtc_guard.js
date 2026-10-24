// SPDX-License-Identifier: MIT
//
// WebRTC is switched off inside a site page.
//
// WHY: the connection-pinning proxy (`DnsPinnedProxy.kt` on Android) is an
// HTTP `CONNECT` proxy, and `CONNECT` carries TCP only. Chromium's WebRTC stack
// gathers ICE candidates over UDP and, per RFC 8828 §5.1 principle 4, does NOT
// send that traffic through a configured HTTP proxy by default — so a page that
// opens an `RTCPeerConnection` can create traffic that never reaches the proxy
// and is therefore never pinned to a classified address. Removing the API is
// the only mechanism a page-level script can offer for that traffic.
//
// WHAT THIS IS NOT: a security boundary. A page runs in the same realm as this
// script but the document-start hook can be missed (a provider without
// `DOCUMENT_START_SCRIPT` falls back to `onPageStarted`, which can lose the race
// with an inline `<script>`), and platform behaviour is not ours to guarantee.
// The real enforcement for "no traffic outside the pinned path" is a network
// layer (a `VpnService`-based filter with user consent); until that exists,
// Android sites are pinned for HTTP(S)/WSS and NOT for WebRTC. That is stated
// in `docs/security-residual-risks.md` and checked on the device checklist
// (packet capture while a page creates a peer connection).
//
// The override is deliberately non-configurable and non-writable: a page that
// tries to `delete` or reassign it fails silently instead of restoring the
// platform constructor.
(function () {
  var blocked = function () {
    var err = new Error(
      "WebRTC is disabled in AnimeHub: it would bypass the pinned network path",
    );
    err.name = "NotSupportedError";
    err.code = 9;
    throw err;
  };
  blocked.prototype = {};
  var names = ["RTCPeerConnection", "webkitRTCPeerConnection"];
  for (var i = 0; i < names.length; i++) {
    try {
      Object.defineProperty(window, names[i], {
        value: blocked,
        configurable: false,
        writable: false,
        enumerable: false,
      });
    } catch (e) {
      // An old provider can refuse redefinition; assignment is the next best
      // thing, and it is still better than leaving the API in place.
      try {
        window[names[i]] = blocked;
      } catch (e2) {
        // Nothing else can be done from a page script. Documented as open.
      }
    }
  }
})();
