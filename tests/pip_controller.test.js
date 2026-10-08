/**
 * Behavioural tests for the shipped WebView PiP controller
 * (`src-tauri/src/web/pip_controller.js`).
 *
 * The controller is the piece that decides *which* player Android's PiP window
 * shows and what the page looks like while PiP is up, so it is tested here
 * against a fake DOM instead of only by hand on a device. Each test maps to one
 * of the scenarios in the task list (no video, several videos, a small ad,
 * a cross-origin iframe player, fullscreen, restore, SPA re-render, ...).
 */

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { createFakeDom, add, addDiv, addIframe, addVideo, place } from "./helpers/fake_dom.js";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const CONTROLLER_PATH = join(ROOT, "src-tauri", "src", "web", "pip_controller.js");
const CONTROLLER_SRC = readFileSync(CONTROLLER_PATH, "utf8");

// Phone-like defaults: a 360x800 CSS viewport at devicePixelRatio 3.
function phone(extra = {}) {
  const dom = createFakeDom({ width: 360, height: 800, devicePixelRatio: 3, ...extra });
  const sandbox = { window: dom.window, document: dom.document, console };
  vm.createContext(sandbox);
  new vm.Script(CONTROLLER_SRC, { filename: "pip_controller.js" }).runInContext(sandbox);
  dom.prepare = () => plain(dom.window.__animehubPreparePip());
  dom.pip = (active) => dom.window.__animehubPip(active);
  return dom;
}

// Values cross the vm realm boundary, so compare plain data.
const plain = (value) => JSON.parse(JSON.stringify(value));

const styleOf = (el) => el.getAttribute("style") || "";
const hasInline = (el, prop) => styleOf(el).includes(`${prop}:`);

test("installs the documented window API", () => {
  const dom = phone();
  assert.equal(typeof dom.window.__animehubPreparePip, "function");
  assert.equal(typeof dom.window.__animehubPip, "function");
  assert.equal(dom.window.__animehubPipVersion, 1);
  assert.equal(dom.pip(), false, "no PiP view before prepare()");
});

// 1 ------------------------------------------------------------------------
test("no video and no player iframe: prepare fails and leaves the page alone", () => {
  const dom = phone();
  addDiv(dom.document, dom.document.body, { rect: { x: 0, y: 0, width: 360, height: 120 } });

  const result = dom.prepare();
  assert.equal(result.ok, false);
  assert.equal(result.reason, "no-candidate");
  assert.equal(dom.document.body.getAttribute("style"), null);
  assert.equal(dom.document.documentElement.getAttribute("style"), null);
  assert.equal(dom.pip(false), false, "nothing to restore");
  assert.equal(dom.document.getElementById("__animehub-pip"), null);
});

// 2 ------------------------------------------------------------------------
test("a single playing video is selected, with its real aspect ratio and rect", () => {
  const dom = phone();
  const video = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 100, width: 360, height: 203 },
    videoWidth: 1920,
    videoHeight: 1080,
  });

  const result = dom.prepare();
  assert.equal(result.ok, true);
  assert.equal(result.kind, "video");
  assert.equal(result.num, 16);
  assert.equal(result.den, 9);
  // sourceRectHint comes back in device pixels (CSS px * devicePixelRatio).
  assert.deepEqual(result.rect, { left: 0, top: 300, width: 1080, height: 609 });
  assert.match(styleOf(video), /object-fit: contain !important/);
  assert.match(styleOf(video), /width: 100% !important/);
  assert.equal(video.classList.contains("animehub-pip-target"), true);
  assert.equal(dom.pip(), true);
});

// 3 ------------------------------------------------------------------------
test("with several videos the playing one wins, and the largest of those when both play", () => {
  const dom = phone();
  const main = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 60, width: 360, height: 203 },
  });
  const other = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 300, width: 360, height: 203 },
    paused: true,
    currentTime: 0,
  });

  const result = dom.prepare();
  assert.equal(result.ok, true);
  assert.equal(result.kind, "video");
  assert.equal(main.classList.contains("animehub-pip-target"), true);
  assert.equal(other.classList.contains("animehub-pip-target"), false);
  assert.equal(hasInline(other, "object-fit"), false, "the losing video is untouched");
});

// 4 ------------------------------------------------------------------------
test("a small autoplaying ad does not outrank the large main video", () => {
  const dom = phone();
  const main = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 40, width: 360, height: 203 },
  });
  const ad = addVideo(dom.document, dom.document.body, {
    rect: { x: 20, y: 700, width: 320, height: 50 },
  });

  const result = dom.prepare();
  assert.equal(result.kind, "video");
  assert.equal(main.classList.contains("animehub-pip-target"), true);
  assert.equal(ad.classList.contains("animehub-pip-target"), false);
});

test("a paused main video still beats an autoplaying banner", () => {
  const dom = phone();
  const main = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 40, width: 360, height: 203 },
    paused: true,
  });
  const ad = addVideo(dom.document, dom.document.body, {
    rect: { x: 10, y: 720, width: 320, height: 50 },
  });

  const result = dom.prepare();
  assert.equal(main.classList.contains("animehub-pip-target"), true);
  assert.equal(ad.classList.contains("animehub-pip-target"), false);
});

// 4b -----------------------------------------------------------------------
test("a video covering most of the viewport is treated as the fullscreen player", () => {
  const dom = phone();
  // 360x600 of a 360x800 viewport: a fullscreen-style player that never called
  // the Fullscreen API. It is paused, because that is what isolates the rule:
  // only the "fills the screen" bonus can put it ahead of the playing clip.
  const main = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 600 },
    paused: true,
  });
  const smaller = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 640, width: 360, height: 160 },
  });

  const result = dom.prepare();
  assert.equal(result.ok, true);
  assert.equal(main.classList.contains("animehub-pip-target"), true);
  assert.equal(smaller.classList.contains("animehub-pip-target"), false);
});

// 5 ------------------------------------------------------------------------
test("a cross-origin iframe player is used as the fallback surface", () => {
  const dom = phone();
  addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 10, width: 1, height: 1 },
    readyState: 0,
    paused: true,
    currentTime: 0,
    videoWidth: 0,
    videoHeight: 0,
  });
  const frame = addIframe(dom.document, dom.document.body, {
    rect: { x: 0, y: 60, width: 360, height: 210 },
    src: "https://cdn.other-origin.example/player/embed/123",
    allow: "autoplay; fullscreen; picture-in-picture; encrypted-media",
  });

  const result = dom.prepare();
  assert.equal(result.ok, true);
  assert.equal(result.kind, "iframe");
  assert.equal(frame.classList.contains("animehub-pip-target"), true);
  assert.equal(result.num > 0 && result.den > 0, true);
  assert.ok(result.num / result.den > 1.2 && result.num / result.den < 2.1, `${result.num}/${result.den}`);
});

test("a playing video beats a player-shaped iframe of the same size", () => {
  const dom = phone();
  const video = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 60, width: 360, height: 203 },
  });
  addIframe(dom.document, dom.document.body, {
    rect: { x: 0, y: 60, width: 360, height: 203 },
    src: "https://cdn.example/player/embed/1",
    allow: "autoplay; fullscreen",
  });

  const result = dom.prepare();
  assert.equal(result.kind, "video");
  assert.equal(video.classList.contains("animehub-pip-target"), true);
});

test("a screen-filling iframe beats a tiny autoplaying clip", () => {
  const dom = phone();
  const ad = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 10, width: 320, height: 60 },
  });
  const frame = addIframe(dom.document, dom.document.body, {
    rect: { x: 0, y: 90, width: 360, height: 500 },
    src: "https://cdn.example/embed/video/9",
    allow: "autoplay; fullscreen",
  });

  const result = dom.prepare();
  assert.equal(result.kind, "iframe");
  assert.equal(frame.classList.contains("animehub-pip-target"), true);
  assert.equal(ad.classList.contains("animehub-pip-target"), false);
});

// 6 / 9 --------------------------------------------------------------------
test("a fullscreen video inside a wrapper is preferred and the wrapper becomes the stage", () => {
  const dom = phone();
  const wrapper = addDiv(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 800 },
  });
  const controls = addDiv(dom.document, wrapper, { rect: { x: 0, y: 700, width: 360, height: 60 } });
  const video = addVideo(dom.document, wrapper, {
    rect: { x: 0, y: 0, width: 360, height: 202 },
    computed: { position: "static" },
  });
  dom.document.fullscreenElement = wrapper;

  const result = dom.prepare();
  assert.equal(result.ok, true);
  assert.equal(result.fullscreen, true);
  assert.equal(video.classList.contains("animehub-pip-target"), true);
  assert.equal(wrapper.classList.contains("animehub-pip-stage"), true);
  assert.match(styleOf(wrapper), /position: fixed !important/);
  assert.equal(hasInline(controls, "visibility"), false, "the player's own controls stay visible");
});

test("fullscreen priority does not depend on any site class or id", () => {
  const dom = phone();
  const wrapper = addDiv(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 800 },
    id: "whatever-the-site-calls-it",
  });
  addVideo(dom.document, wrapper, { rect: { x: 0, y: 0, width: 360, height: 202 } });
  const elsewhere = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 202 },
  });
  dom.document.fullscreenElement = wrapper;

  dom.prepare();
  assert.equal(wrapper.classList.contains("animehub-pip-stage"), true);
  assert.equal(elsewhere.classList.contains("animehub-pip-target"), false);
});

// 7 / 8 --------------------------------------------------------------------
test("prepare never moves the player in the DOM and keeps playback state", () => {
  const dom = phone();
  const wrapper = addDiv(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 220 },
  });
  const video = addVideo(dom.document, wrapper, {
    rect: { x: 0, y: 0, width: 360, height: 203 },
  });
  const parentBefore = video.parentNode;
  const childrenBefore = dom.document.body.children.slice();

  dom.prepare();

  assert.equal(video.parentNode, parentBefore, "the video is not reparented");
  assert.deepEqual(dom.document.body.children, childrenBefore, "no elements are added or removed");
  assert.equal(video.paused, false, "playback is untouched");
  assert.equal(video.currentTime, 30);
});

test("the rest of the page stops painting while the player chain stays visible", () => {
  const dom = phone();
  const header = addDiv(dom.document, dom.document.body, { rect: { x: 0, y: 0, width: 360, height: 40 } });
  const wrapper = addDiv(dom.document, dom.document.body, {
    rect: { x: 0, y: 40, width: 360, height: 220 },
  });
  const controls = addDiv(dom.document, wrapper, { rect: { x: 0, y: 40, width: 360, height: 17 } });
  addVideo(dom.document, wrapper, { rect: { x: 0, y: 40, width: 360, height: 203 } });

  dom.prepare();
  assert.match(styleOf(header), /visibility: hidden !important/);
  assert.equal(hasInline(controls, "visibility"), false, "controls inside the stage stay visible");
  assert.equal(hasInline(dom.document.body, "overflow"), true);
  assert.match(styleOf(dom.document.body), /background-color: #000 !important/);
});

// 11 -----------------------------------------------------------------------
test("restore puts every inline style and class back, exactly", () => {
  const dom = phone();
  const bodyStyleBefore = "color: red";
  const wrapper = addDiv(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 220 },
    style: "padding: 4px",
  });
  const video = addVideo(dom.document, wrapper, {
    rect: { x: 0, y: 0, width: 360, height: 203 },
    style: undefined,
  });
  dom.document.body.setAttribute("style", bodyStyleBefore);
  const videoStyleBefore = video.getAttribute("style");
  assert.equal(videoStyleBefore, null);

  assert.equal(dom.prepare().ok, true);
  assert.notEqual(bodyStyleBefore, dom.document.body.getAttribute("style"));
  assert.equal(dom.pip(false), true);

  assert.equal(dom.document.body.getAttribute("style"), bodyStyleBefore);
  assert.equal(dom.document.documentElement.getAttribute("style"), null);
  assert.equal(wrapper.getAttribute("style"), "padding: 4px");
  assert.equal(video.getAttribute("style"), null);
  assert.equal(dom.document.documentElement.getAttribute("data-animehub-pip"), null);
  assert.equal(video.classList.contains("animehub-pip-target"), false);
  assert.equal(wrapper.classList.contains("animehub-pip-stage"), false);
  assert.equal(dom.pip(false), false, "second restore is a no-op");
  assert.equal(dom.pip(), false);
});

test("a site's own classes survive the round trip", () => {
  const dom = phone();
  const video = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 203 },
  });
  video.classList.add("vjs-tech");

  dom.prepare();
  dom.pip(false);
  assert.equal(video.classList.contains("vjs-tech"), true);
  assert.equal(video.classList.contains("animehub-pip-target"), false);
});

test("__animehubPip(true) re-asserts the view without losing the selection", () => {
  const dom = phone();
  const video = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 203 },
  });

  dom.prepare();
  assert.equal(dom.pip(true), true);
  assert.match(styleOf(video), /object-fit: contain !important/);
  assert.equal(dom.pip(false), true);
  assert.equal(styleOf(video), "");
});

// 10 -----------------------------------------------------------------------
test("a SPA that replaces the player mid-PiP cannot break the restore", () => {
  const dom = phone();
  const wrapper = addDiv(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 220 },
  });
  const first = addVideo(dom.document, wrapper, { rect: { x: 0, y: 0, width: 360, height: 203 } });
  dom.prepare();
  assert.equal(first.classList.contains("animehub-pip-target"), true);

  // The framework re-renders its player: the prepared node leaves the tree.
  wrapper.removeChild(first);
  const second = addVideo(dom.document, wrapper, { rect: { x: 0, y: 0, width: 360, height: 203 } });

  assert.equal(dom.pip(false), true, "restore still succeeds");
  assert.equal(dom.document.body.getAttribute("style"), null);
  assert.equal(second.classList.contains("animehub-pip-target"), false);

  // And the controller is reusable on the re-rendered page.
  const again = dom.prepare();
  assert.equal(again.ok, true);
  assert.equal(second.classList.contains("animehub-pip-target"), true);
  assert.equal(dom.pip(false), true);
});

test("prepare is re-entrant: a second call never stacks a view", () => {
  const dom = phone();
  const video = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 203 },
  });
  dom.prepare();
  dom.prepare();
  assert.equal(dom.document.body.getAttribute("style") === null, false);
  assert.equal(dom.pip(false), true);
  assert.equal(dom.document.body.getAttribute("style"), null);
});

// 12 -----------------------------------------------------------------------
test("hidden or degenerate videos produce no candidate", () => {
  const dom = phone();
  addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 203 },
    computed: { display: "none" },
  });
  addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 0, height: 0 },
  });
  addVideo(dom.document, dom.document.body, {
    rect: { x: -400, y: -400, width: 10, height: 10 },
  });

  const result = dom.prepare();
  assert.equal(result.ok, false);
  assert.equal(result.reason, "no-candidate");
  assert.equal(dom.document.body.getAttribute("style"), null);
});

test("a video hidden behind an off-screen scroll position is not selected", () => {
  const dom = phone();
  const wrapper = addDiv(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 200 },
  });
  addVideo(dom.document, wrapper, { rect: { x: 0, y: -900, width: 360, height: 200 } });
  addVideo(dom.document, wrapper, { rect: { x: 0, y: 40, width: 360, height: 200 } });

  const result = dom.prepare();
  assert.equal(result.ok, true);
  assert.equal(result.kind, "video");
});

// 14 -----------------------------------------------------------------------
test("invalid intrinsic sizes fall back to a valid 16:9 request", () => {
  const dom = phone();
  addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 203 },
    videoWidth: 0,
    videoHeight: 0,
  });
  const result = dom.prepare();
  assert.equal(result.ok, true);
  assert.equal(result.num, 16);
  assert.equal(result.den, 9);
});

test("an extreme aspect ratio never leaves the range Android accepts", () => {
  const dom = phone();
  addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 60 },
    videoWidth: 8000,
    videoHeight: 500,
  });
  const result = dom.prepare();
  assert.equal(result.ok, true);
  const ratio = result.num / result.den;
  assert.ok(ratio <= 2.39 && ratio >= 1 / 2.39, `${result.num}/${result.den}`);
});

// 20 / security ------------------------------------------------------------
test("the controller never reaches into iframes and never uses an HTML sink", () => {
  // Comments describe the rules, so the behavioural scan runs on code only.
  const code = CONTROLLER_SRC.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");
  assert.equal(/contentDocument|contentWindow/.test(code), false, "same-origin policy is never touched");
  assert.equal(/innerHTML|outerHTML|insertAdjacentHTML|document\.write/.test(code), false);
  assert.equal(/eval\s*\(|new Function/.test(code), false);
  assert.equal(/xmlhttprequest|fetch\s*\(/i.test(code), false, "no network access from the injected script");
  // Embedding constraints apply to the whole file, comments included:
  // scripts/android_prepare.py puts the text inside a Kotlin raw string, where
  // `$` interpolates and `"""` would end the literal.
  assert.equal(/\$/.test(CONTROLLER_SRC), false, "no template literals or jQuery-style dollars");
  assert.equal(CONTROLLER_SRC.includes('"""'), false, "no triple quotes: Kotlin raw string delimiter");
});

test("a page with a broken candidate list does not leave a half-styled page", () => {
  const dom = phone();
  const video = addVideo(dom.document, dom.document.body, {
    rect: { x: 0, y: 0, width: 360, height: 203 },
  });
  // A hostile/broken page: writing styles throws.
  video.style.setProperty = () => {
    throw new Error("nope");
  };
  const result = dom.prepare();
  // Either the view was applied without that property or the page was restored;
  // in both cases the document must not be left with our root styles.
  if (!result.ok) {
    assert.equal(dom.document.documentElement.getAttribute("style"), null);
  }
  assert.equal(typeof dom.pip(false), "boolean");
  dom.pip(false);
  assert.equal(video.getAttribute("style"), null);
});

void place;
