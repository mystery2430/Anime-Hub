/**
 * Behavioural tests for the script AnimeHub injects into every site WebView
 * frame (AH-P2).
 *
 * The script is generated in Rust by `build_init_script`
 * (`src-tauri/src/web/session.rs`) from three pieces: the popup guard
 * (`web/popup_guard.js`), a cosmetic `<style>` built from the injected config,
 * and — on Android only — the PiP controller (`web/pip_controller.js`). Rust
 * registers the result with `addDocumentStartJavaScript(.., setOf("*"))`, so it
 * runs in EVERY frame of EVERY origin; what has to be true is that no frame
 * gets a capability out of it.
 *
 * TWO KINDS OF ASSERTION, KEPT APART:
 *  1. BEHAVIOUR: the shipped JavaScript files are executed in a Node `vm`
 *     against a fake DOM. These tests prove what the guard does to the fake
 *     window and that unrelated media/fetch APIs keep their identity. They do
 *     NOT run a WebView, open a socket, capture packets or test a device.
 *  2. SOURCE PINS: some tests read Rust/Kotlin/JS source and assert that the
 *     wiring still mentions the shipped file. They catch accidental deletion
 *     from a call site; they are not evidence that network traffic is blocked.
 *
 * The PiP controller's own frame gate is tested in `pip_controller.test.js`,
 * against the same file.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { createFakeDom, asSubframe } from "./helpers/fake_dom.js";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (rel) => readFileSync(join(ROOT, rel), "utf8").replace(/\r\n/g, "\n");

const SESSION_RS = read("src-tauri/src/web/session.rs");
const POPUP_GUARD = read("src-tauri/src/web/popup_guard.js");
const PIP_CONTROLLER = read("src-tauri/src/web/pip_controller.js");
const WEBRTC_GUARD = read("src-tauri/src/web/webrtc_guard.js");
const WINDOWS_RS = read("src-tauri/src/web/windows.rs");
const COMMANDS_RS = read("src-tauri/src/commands.rs");

const CFG = { blockPopups: true, hideSelectors: [".ad-container", "#popup"], pipController: true };

/**
 * Compose the injected script the way `build_init_script` does: the same
 * wrapper, the same three pieces, the same order. `session.rs` is pinned below
 * so this mirror cannot drift from what ships.
 */
function compose({ popup = true, pip = true, webrtc = false } = {}) {
  const cosmetic = `
  var style = document.createElement("style");
  style.id = "animehub-cosmetic";
  style.textContent = CFG.hideSelectors.join(",") + "{display:none !important;}";
  (document.head || document.documentElement).appendChild(style);
`;
  return `(function () {
  "use strict";
  if (window.__animehubInstalled) return;
  window.__animehubInstalled = true;
  var CFG = ${JSON.stringify(CFG)};
${popup ? POPUP_GUARD : ""}${webrtc ? WEBRTC_GUARD : ""}${cosmetic}${pip ? PIP_CONTROLLER : ""}})();`;
}

function run(dom, script) {
  const sandbox = { window: dom.window, document: dom.document, console };
  vm.createContext(sandbox);
  new vm.Script(script, { filename: "init_script.js" }).runInContext(sandbox);
  return dom;
}

/** A stand-in for the platform's own `window.open`, which the guard replaces. */
function nativeOpen() {
  return { closed: false, focus() {} };
}

test("the popup guard neutralises window.open in the top frame and in a subframe", () => {
  for (const [label, prepare] of [
    ["top frame", (dom) => dom],
    ["cross-origin subframe", asSubframe],
  ]) {
    const dom = prepare(createFakeDom({ width: 360, height: 800 }));
    const original = nativeOpen;
    dom.window.open = original;

    run(dom, compose());

    assert.equal(typeof dom.window.open, "function", `${label}: still a function`);
    assert.equal(dom.window.open("https://player.example/next"), null, `${label}: returns null`);
    assert.equal(dom.window.open(), null, `${label}: no arguments, still null`);
    assert.notEqual(dom.window.open, original, `${label}: replaced, not wrapped`);

    // The removed escape hatch: the guard must not keep the original reachable
    // in any way a frame could find. It used to sit on a non-enumerable
    // property of the replacement, which every cross-origin frame could read.
    const own = Object.getOwnPropertyNames(dom.window.open);
    for (const name of ["length", "name", "prototype"]) {
      assert.ok(own.includes(name), `${label}: a plain function keeps ${name}`);
    }
    for (const name of own) {
      assert.notEqual(
        dom.window.open[name],
        original,
        `${label}: window.open.${name} still reaches the native open`,
      );
    }
    assert.deepEqual(
      own.filter((n) => !["length", "name", "prototype"].includes(n)),
      [],
      `${label}: the override carries no extra properties`,
    );
    for (const key of Object.keys(dom.window)) {
      assert.notEqual(
        dom.window[key],
        original,
        `${label}: window.${key} exposes the native window.open`,
      );
    }
    // The page-start fallback (providers without DOCUMENT_START_SCRIPT)
    // evaluates the same script in the same document: the install flag must
    // make that a no-op rather than stacking a second override.
    const applied = dom.window.open;
    run(dom, compose());
    assert.equal(dom.window.open, applied, `${label}: never applied twice`);
    assert.equal(dom.window.open("https://player.example/x"), null, `${label}: still guarded`);
  }
});

test("the WebRTC guard removes the API in the top frame and in a subframe", () => {
  // BEHAVIOUR (fake DOM, not a WebView or network test). The guard does not
  // read window.top, so applying it to any frame must not depend on the frame
  // being top-level.
  for (const [label, prepare] of [
    ["top frame", (dom) => dom],
    ["cross-origin subframe", asSubframe],
  ]) {
    const dom = prepare(createFakeDom({ width: 360, height: 800 }));
    let peerConnections = 0;
    let transports = 0;
    class NativePeerConnection {
      constructor() {
        peerConnections += 1;
      }
    }
    class NativeWebTransport {
      constructor() {
        transports += 1;
      }
    }
    dom.window.RTCPeerConnection = NativePeerConnection;
    dom.window.webkitRTCPeerConnection = NativePeerConnection;
    dom.window.WebTransport = NativeWebTransport;

    run(dom, compose({ webrtc: true }));

    for (const name of ["RTCPeerConnection", "webkitRTCPeerConnection", "WebTransport"]) {
      const value = dom.window[name];
      assert.equal(typeof value, "function", `${label}: ${name} is still callable`);
      assert.notEqual(value, NativePeerConnection, `${label}: ${name} is replaced`);
      assert.notEqual(value, NativeWebTransport, `${label}: ${name} is replaced`);
      // Calling it must refuse loudly instead of silently returning null.
      assert.throws(
        () => new value(),
        (error) => error && error.name === "NotSupportedError",
        `${label}: ${name} must refuse with NotSupportedError`,
      );
      assert.throws(
        () => value(),
        (error) => error && error.name === "NotSupportedError",
        `${label}: ${name} must refuse without new too`,
      );
      // The original constructor is unreachable: the property cannot be
      // reassigned, redefined or deleted by the page.
      const descriptor = Object.getOwnPropertyDescriptor(dom.window, name);
      assert.equal(descriptor.configurable, false, `${label}: ${name} is not configurable`);
      assert.equal(descriptor.writable, false, `${label}: ${name} is not writable`);
      assert.throws(
        () => {
          "use strict";
          dom.window[name] = NativePeerConnection;
        },
        TypeError,
        `${label}: ${name} cannot be reassigned`,
      );
      assert.throws(
        () => Object.defineProperty(dom.window, name, { value: NativePeerConnection }),
        TypeError,
        `${label}: ${name} cannot be redefined`,
      );
      assert.throws(
        () => {
          "use strict";
          delete dom.window[name];
        },
        TypeError,
        `${label}: ${name} cannot be deleted`,
      );
    }
    // Applying it twice (document-start + a page-start fallback) must not
    // stack a second override or re-expose anything.
    const applied = dom.window.RTCPeerConnection;
    run(dom, compose({ webrtc: true }));
    assert.equal(dom.window.RTCPeerConnection, applied, `${label}: never applied twice`);
    assert.equal(peerConnections, 0, `${label}: the native constructor never ran`);
    assert.equal(transports, 0, `${label}: the native WebTransport constructor never ran`);
  }
});

test("the guard leaves MSE, media and other egress APIs untouched", () => {
  // BEHAVIOUR (fake DOM only). WebRTC is not on the MSE/HLS/DASH playback
  // path. The test stubs the browser-shaped APIs and checks that the guard
  // neither replaces nor wraps them. It cannot prove a real video plays.
  const dom = createFakeDom({ width: 360, height: 800 });
  const NativePeerConnection = function NativePeerConnection() {};
  const MediaSource = function MediaSource() {};
  MediaSource.isTypeSupported = (type) => type === "video/mp4";
  const SourceBuffer = function SourceBuffer() {};
  const play = function play() {};
  const load = function load() {};
  const createObjectURL = function createObjectURL() {};
  const revokeObjectURL = function revokeObjectURL() {};
  const getUserMedia = function getUserMedia() {};
  const fetch = function fetch() {};
  const WebSocket = function WebSocket() {};

  dom.window.RTCPeerConnection = NativePeerConnection;
  dom.window.MediaSource = MediaSource;
  dom.window.SourceBuffer = SourceBuffer;
  dom.window.HTMLMediaElement = { prototype: { play, load } };
  dom.window.URL = { createObjectURL, revokeObjectURL };
  dom.window.navigator = { mediaDevices: { getUserMedia } };
  dom.window.fetch = fetch;
  dom.window.WebSocket = WebSocket;

  run(dom, compose({ webrtc: true }));

  assert.notEqual(dom.window.RTCPeerConnection, NativePeerConnection, "WebRTC is blocked");
  assert.equal(dom.window.MediaSource, MediaSource, "MSE is not replaced");
  assert.equal(dom.window.MediaSource.isTypeSupported("video/mp4"), true, "MSE probing still works");
  assert.equal(dom.window.SourceBuffer, SourceBuffer, "SourceBuffer is not replaced");
  assert.equal(dom.window.HTMLMediaElement.prototype.play, play, "media play is not wrapped");
  assert.equal(dom.window.HTMLMediaElement.prototype.load, load, "media load is not wrapped");
  assert.equal(dom.window.URL.createObjectURL, createObjectURL, "MSE blob URLs still work");
  assert.equal(dom.window.URL.revokeObjectURL, revokeObjectURL, "MSE blob URLs can still be revoked");
  assert.equal(dom.window.navigator.mediaDevices.getUserMedia, getUserMedia);
  assert.equal(dom.window.fetch, fetch, "fetch keeps the platform implementation");
  assert.equal(dom.window.WebSocket, WebSocket, "WebSocket keeps the platform implementation");
});

test("WebTransport is only replaced where the provider exposes it", () => {
  // BEHAVIOUR (fake DOM). Defining the constructor on an older browser would
  // make feature detection dishonest and could send a player down a path it
  // cannot support.
  const dom = createFakeDom({ width: 360, height: 800 });
  assert.equal("WebTransport" in dom.window, false, "precondition: provider has no WebTransport");
  run(dom, compose({ webrtc: true }));
  assert.equal("WebTransport" in dom.window, false, "no fake WebTransport constructor is invented");
  assert.equal(dom.window.WebTransport, undefined);
  assert.throws(() => new dom.window.RTCPeerConnection(), (e) => e.name === "NotSupportedError");
});

test("source pin: the shipped guard is wired once per platform for every frame", () => {
  const dom = createFakeDom({ width: 360, height: 800 });
  const NativePeerConnection = function NativePeerConnection() {};
  dom.window.RTCPeerConnection = NativePeerConnection;
  run(dom, compose({ webrtc: false }));
  assert.equal(
    dom.window.RTCPeerConnection,
    NativePeerConnection,
    "the composed script alone never overrides the API",
  );
  assert.ok(WEBRTC_GUARD.includes("RTCPeerConnection"), "the shipped file is the override");
  // The composition is the only thing that decides, and it does so behind a
  // native-side flag: Android sets it and gets the guard inside the
  // document-start script (which its provider applies to every frame).
  const production = SESSION_RS.split("mod tests")[0];
  assert.match(production, /include_str!\("webrtc_guard\.js"\)/);
  assert.match(production, /if cfg\.webrtc_guard/);
  assert.match(
    COMMANDS_RS,
    /webrtc_guard: cfg!\(target_os = "android"\)/,
    "the Android open path composes the guard into the document-start script",
  );
  // Desktop has a pinned path too (the Rust CONNECT proxy) and installs the
  // same shipped file for every frame instead of duplicating it into the
  // top-frame script. If this pin ever loses the registration, desktop would
  // silently keep `RTCPeerConnection` while the proxy pins only TCP.
  assert.match(
    WINDOWS_RS,
    /\.initialization_script_for_all_frames\(crate::web::session::WEBRTC_GUARD\)/,
    "the desktop site WebView must install the guard in every frame",
  );
  // SOURCE-ONLY LIMIT: this regex proves that the call site still exists. It
  // does not prove that WebView2/WebKitGTK applied the script before page
  // code, that the provider honoured it, or that no UDP/QUIC packet escaped.
  // Android additionally gates on DOCUMENT_START_SCRIPT in the controller;
  // desktop has no equivalent runtime attestation.
});

test("source pin: the popup guard keeps no reference to the original function", () => {
  assert.match(POPUP_GUARD, /window\.open = function/, "the override is the shipped text");
  assert.ok(!POPUP_GUARD.includes("__animehubNative"), "no escape-hatch property name");
  assert.ok(!POPUP_GUARD.includes("nativeOpen"), "no captured reference");
  assert.ok(
    !POPUP_GUARD.includes("defineProperty(window.open"),
    "the override is not decorated with properties",
  );
  // The generator's own (non-test) half must not rebuild the guard inline: the
  // shipped text comes from popup_guard.js, so the checks above are about what
  // actually runs. The test half legitimately names the removed property in
  // order to assert its absence, hence the split.
  const production = SESSION_RS.split("mod tests")[0];
  assert.ok(production.length > 1000, "found the generator above the test module");
  assert.ok(
    !production.includes("defineProperty(window.open"),
    "the generator must not decorate window.open again",
  );
  assert.ok(!production.includes("nativeOpen"), "no captured original in the generator");
});

test("turning popups off ships no window.open override at all", () => {
  const without = compose({ popup: false, pip: false });
  assert.ok(!without.includes("window.open"), "the guard is omitted, not disabled");
  const dom = createFakeDom({ width: 360, height: 800 });
  const original = nativeOpen;
  dom.window.open = original;
  run(dom, without);
  assert.equal(dom.window.open, original, "an unguarded view keeps the platform behaviour");
});

test("the composed script stays inert for every origin it runs in", () => {
  const script = compose().toLowerCase();
  for (const needle of [
    "token",
    "secret",
    "cookie",
    "authorization",
    "bearer",
    "client_id",
    "client_secret",
    "accesstoken",
    "http://",
    "https://",
    "profile",
    "openani",
    "animecix",
    "myanimelist",
  ]) {
    assert.ok(!script.includes(needle), `the injected text must stay inert, but mentions ${needle}`);
  }
  assert.ok(!compose().includes("</script>"), "no early script termination");
  // The payload is the three display settings and nothing per-site.
  for (const key of ["blockPopups", "hideSelectors", "pipController"]) {
    assert.ok(compose().includes(key), `payload lost ${key}`);
  }
});

test("the cosmetic rules still reach a subframe document", () => {
  // Narrowing the frame scope must not lose the ad-hiding: ad containers live
  // inside third-party frames, and a style element is data, not a capability.
  const dom = asSubframe(createFakeDom({ width: 360, height: 800 }));
  run(dom, compose({ pip: false }));
  // The injected script sets `style.id` as a property, which is how a real DOM
  // reflects it into the attribute; the fake DOM keeps them apart, so read the
  // element back from the head it was appended to.
  const style = dom.document.head.children.find((el) => el.id === "animehub-cosmetic");
  assert.ok(style, "the subframe got the cosmetic style");
  assert.match(style.textContent, /\.ad-container/);
  assert.match(style.textContent, /display:none !important/);
  assert.equal(
    dom.document.head.children.filter((el) => el.id === "animehub-cosmetic").length,
    1,
    "exactly one style element per document",
  );
});

test("source pin: session.rs composes exactly these files, in this order", () => {
  // This is the pin that makes the tests above about the SHIPPED script: the
  // generator has to keep interpolating the popup guard, the cosmetic rules and
  // the controller from these files, in the order they are composed here.
  assert.match(SESSION_RS, /include_str!\("popup_guard\.js"\)/);
  assert.match(SESSION_RS, /include_str!\("pip_controller\.js"\)/);
  assert.match(
    SESSION_RS,
    /include_str!\("webrtc_guard\.js"\)/,
    "the shipped WebRTC guard must come from its own file",
  );
  const wrapper = SESSION_RS.indexOf("(function () {{");
  assert.ok(wrapper > 0, "the generated wrapper is missing");
  const template = SESSION_RS.slice(wrapper);
  const payloadAt = template.indexOf("{payload}");
  const popupAt = template.indexOf("{popup_guard}");
  const cosmeticAt = template.indexOf("{cosmetic}");
  const pipAt = template.indexOf("{pip_controller}");
  const webrtcAt = template.indexOf("{webrtc_guard}");
  assert.ok(webrtcAt > popupAt, "the WebRTC guard runs after the popup guard");
  assert.ok(cosmeticAt > webrtcAt, "and before the cosmetic rules");
  assert.ok(payloadAt > 0, "config payload");
  assert.ok(popupAt > payloadAt, "popup guard after the payload");
  assert.ok(cosmeticAt > popupAt, "cosmetic rules after the popup guard");
  assert.ok(pipAt > cosmeticAt, "PiP controller last");
  assert.ok(
    !/r#"\s*\n\s*\/\/ Neutralise attempts to open new windows/.test(SESSION_RS),
    "no inline copy of the popup guard may come back",
  );
});
