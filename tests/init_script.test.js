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
 * These tests execute the shipped files (not a paraphrase) in a fake DOM:
 *  - the popup guard must neutralise `window.open` in the top frame AND in a
 *    cross-origin subframe, and must not leave the original reachable;
 *  - the composition must stay inert (no credential-looking word, no site
 *    identity, no `</script>` escape);
 *  - `session.rs` must still compose exactly these files, in this order, so a
 *    future inline copy cannot quietly reintroduce what was removed here.
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

const CFG = { blockPopups: true, hideSelectors: [".ad-container", "#popup"], pipController: true };

/**
 * Compose the injected script the way `build_init_script` does: the same
 * wrapper, the same three pieces, the same order. `session.rs` is pinned below
 * so this mirror cannot drift from what ships.
 */
function compose({ popup = true, pip = true } = {}) {
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
${popup ? POPUP_GUARD : ""}${cosmetic}${pip ? PIP_CONTROLLER : ""}})();`;
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

test("the popup guard source keeps no reference to the original function", () => {
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

test("session.rs composes exactly these files, in this order", () => {
  // This is the pin that makes the tests above about the SHIPPED script: the
  // generator has to keep interpolating the popup guard, the cosmetic rules and
  // the controller from these files, in the order they are composed here.
  assert.match(SESSION_RS, /include_str!\("popup_guard\.js"\)/);
  assert.match(SESSION_RS, /include_str!\("pip_controller\.js"\)/);
  const wrapper = SESSION_RS.indexOf("(function () {{");
  assert.ok(wrapper > 0, "the generated wrapper is missing");
  const template = SESSION_RS.slice(wrapper);
  const payloadAt = template.indexOf("{payload}");
  const popupAt = template.indexOf("{popup_guard}");
  const cosmeticAt = template.indexOf("{cosmetic}");
  const pipAt = template.indexOf("{pip_controller}");
  assert.ok(payloadAt > 0, "config payload");
  assert.ok(popupAt > payloadAt, "popup guard after the payload");
  assert.ok(cosmeticAt > popupAt, "cosmetic rules after the popup guard");
  assert.ok(pipAt > cosmeticAt, "PiP controller last");
  assert.ok(
    !/r#"\s*\n\s*\/\/ Neutralise attempts to open new windows/.test(SESSION_RS),
    "no inline copy of the popup guard may come back",
  );
});
