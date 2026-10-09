/**
 * AnimeHub — Android Picture-in-Picture controller for the site WebView.
 *
 * Android PiP shrinks the *whole* Activity (and therefore AnimeHub's single
 * site WebView) into the small PiP window. Site pages, though, are not bare
 * video players: they carry headers, episode lists, comments, ad overlays and
 * often a second, autoplaying ad clip. So the WebView has to *look* like a
 * player surface at the moment PiP starts, and look like the site again when
 * PiP ends. This file is exactly that piece of state, and nothing else.
 *
 * Public surface (installed on `window`):
 *
 *   window.__animehubPipVersion     integer version of the installed copy
 *   window.__animehubPreparePip()   find the active player, apply the
 *                                   PiP-only view, return a JSON-safe object:
 *                                   { ok, kind, num, den, rect, score, fullscreen }
 *   window.__animehubPip(false)     restore the page exactly as it was
 *   window.__animehubPip(true)      re-assert the current PiP view
 *                                   (orientation / size change inside PiP)
 *   window.__animehubPip()          true while a PiP view is applied
 *
 * Design rules
 * ------------
 *  * **No DOM moves.** `remove()`, `appendChild()`, `replaceChildren()` and
 *    friends would break React/Vue players and kill playback; the selected
 *    player stays exactly where it is in the tree. Only inline CSS is layered
 *    on top, and every element we touch keeps its previous inline `style`
 *    attribute so the restore is byte-exact rather than guessed.
 *  * **No site-specific selectors.** Candidates are scored from live geometry
 *    and playback state, never from a hard-coded class or id of one site.
 *  * **Iframes stay opaque.** A cross-origin frame is only ever used as a
 *    *surface* (bounds, `allow`, `src` shape). This file never touches
 *    `contentDocument` / `contentWindow`: same-origin policy is not bypassed,
 *    and a same-origin frame gets no special treatment either.
 *  * **No IPC.** The controller cannot talk to Rust/Kotlin; it only reads and
 *    styles the page it runs in. The Kotlin side reads one return value.
 *  * **Synchronous.** The Activity must enter PiP in the same turn as the
 *    preparation, so there are no timers, promises or animation frames here.
 *  * **No dollar signs and no triple quotes in this file.** This text is
 *    embedded verbatim inside a Kotlin raw string by
 *    `scripts/android_prepare.py`, where a dollar sign would interpolate and
 *    three quotes would end the literal; `tests/pip_controller.test.js` fails
 *    the build if either sequence shows up.
 */
(function () {
  "use strict";

  // Bump when the applied view changes shape: a document that already has a
  // newer copy refuses to be re-installed with an older one.
  var VERSION = 1;

  // ------------------------------------------------------------------ names
  var PREPARE_FN = "__animehubPreparePip";
  var TOGGLE_FN = "__animehubPip";
  var REPORT_FN = "__animehubPipReport";
  var VERSION_KEY = "__animehubPipVersion";

  // --------------------------------------------------------------- tuning
  // Minimum surface for a candidate to be considered a player at all.
  var MIN_VISIBLE_AREA = 12000; // px^2, roughly a 146x82 clip
  var MIN_COVERAGE = 0.015; // of the viewport
  // A clip that is much smaller than the largest one on the page is far more
  // likely to be an ad than the main video, so its playback state must not let
  // it outrank a large player on its own.
  var TINY_SHARE = 0.25; // of the largest video's visible area
  var SMALL_SHARE = 0.6; // of the largest video's visible area
  var TINY_SHARE_PENALTY = 3.5;
  var SMALL_SHARE_PENALTY = 1.0;
  // A clip that fills most of the visible page is a fullscreen player in all
  // but name, even when the site never calls the Fullscreen API. Only the
  // largest video on the page may claim this: a full-bleed decorative
  // background clip must not take the PiP window from the main player.
  var NEAR_FULL_COVERAGE = 0.7; // of the viewport
  var NEAR_FULL_BONUS = 3;
  var LIFT_DEPTH = 8; // ancestors we are willing to adjust
  var STAGE_MAX_GROWTH = 4; // stage area vs. video area
  var FALLBACK_NUM = 16;
  var FALLBACK_DEN = 9;

  var COMMON_RATIOS = [
    [16, 9],
    [4, 3],
    [21, 9],
    [3, 2],
    [1, 1],
    [9, 16],
    [3, 4],
    [9, 21],
    [2, 3],
  ];

  // Inline properties applied to the lifted player. Values are `important`:
  // site stylesheets use `!important` liberally and an inherited transform on
  // an ancestor would otherwise keep our fixed box from reaching the viewport.
  var STAGE_STYLE = [
    "position", "fixed",
    "left", "0px",
    "top", "0px",
    "width", "100%",
    "height", "100%",
    "min-width", "0px",
    "min-height", "0px",
    "max-width", "none",
    "max-height", "none",
    "margin", "0px",
    "padding", "0px",
    "transform", "none",
    "filter", "none",
    "perspective", "none",
    "will-change", "auto",
    "contain", "none",
    "clip-path", "none",
    "opacity", "1",
    "overflow", "hidden",
    "background-color", "#000",
    "box-sizing", "border-box",
    "transition", "none",
    "z-index", "2147483647",
    // Also correct when the stage *is* the video element (a bare player).
    "object-fit", "contain",
  ];

  // Ancestors between the stage and <body>: they must not clip or trap the
  // fixed box (containing block / stacking context creators).
  var ANCESTOR_STYLE = [
    "transform", "none",
    "filter", "none",
    "perspective", "none",
    "will-change", "auto",
    "contain", "none",
    "clip-path", "none",
    "opacity", "1",
    "visibility", "visible",
  ];

  var TARGET_STYLE = [
    "position", "relative",
    "left", "0px",
    "top", "0px",
    "right", "auto",
    "bottom", "auto",
    "width", "100%",
    "height", "100%",
    "min-width", "0px",
    "min-height", "0px",
    "max-width", "none",
    "max-height", "none",
    "margin", "0px",
    "padding", "0px",
    "border", "0px",
    "transform", "none",
    "object-fit", "contain",
    "background-color", "#000",
    "visibility", "visible",
    "opacity", "1",
    "transition", "none",
  ];

  var SIBLING_STYLE = ["visibility", "hidden"];
  var ROOT_STYLE = [
    "margin", "0px",
    "padding", "0px",
    "background-color", "#000",
    "overflow", "hidden",
  ];

  // ---------------------------------------------------------------- state
  // Everything needed to undo the view: touched elements with their original
  // inline style attribute and the classes we added ourselves.
  var state = null;

  function newAccumulator() {
    return { styles: [], classes: [], chosen: null };
  }

  // ------------------------------------------------------------- utilities

  function toArray(list) {
    var out = [];
    var i;
    if (!list) return out;
    for (i = 0; i < list.length; i++) out.push(list[i]);
    return out;
  }

  function numberOrZero(value) {
    return typeof value === "number" && isFinite(value) ? value : 0;
  }

  function viewportSize() {
    var doc = document.documentElement || {};
    var width = numberOrZero(window.innerWidth) || numberOrZero(doc.clientWidth);
    var height = numberOrZero(window.innerHeight) || numberOrZero(doc.clientHeight);
    return { width: width, height: height };
  }

  function rectOf(el) {
    if (!el || typeof el.getBoundingClientRect !== "function") return null;
    var r;
    try {
      r = el.getBoundingClientRect();
    } catch (e) {
      return null;
    }
    if (!r) return null;
    return {
      left: numberOrZero(r.left),
      top: numberOrZero(r.top),
      right: numberOrZero(r.right),
      bottom: numberOrZero(r.bottom),
      width: numberOrZero(r.width),
      height: numberOrZero(r.height),
    };
  }

  // True when a sibling's box overlaps the video box. A sibling with no
  // measurable box is treated as overlapping, so it is left alone.
  function overlapsRect(a, b) {
    if (!a || !b) return true;
    if (a.width <= 0 || a.height <= 0) return true;
    return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
  }

  function areaOf(rect) {
    if (!rect) return 0;
    if (!(rect.width > 0) || !(rect.height > 0)) return 0;
    return rect.width * rect.height;
  }

  function computedStyle(el) {
    if (typeof window.getComputedStyle !== "function") return null;
    try {
      return window.getComputedStyle(el);
    } catch (e) {
      return null;
    }
  }

  function isDetached(el) {
    if (!el) return true;
    // `isConnected` is the cheapest and the most accurate signal; the
    // `document.contains` fallback covers older WebViews.
    if (typeof el.isConnected === "boolean") return el.isConnected === false;
    return typeof document.contains === "function" && !document.contains(el);
  }

  function isHiddenForView(el) {
    if (isDetached(el)) return true;
    if (typeof el.checkVisibility === "function") {
      try {
        if (!el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) return true;
      } catch (e) {
        // Older `checkVisibility` signatures: fall through to the manual test.
      }
    }
    var cs = computedStyle(el);
    if (!cs) return false;
    if (cs.display === "none" || cs.visibility === "hidden" || cs.visibility === "collapse") {
      return true;
    }
    var opacity = parseFloat(cs.opacity);
    if (!isNaN(opacity) && opacity <= 0.05) return true;
    return false;
  }

  function fullscreenElement() {
    try {
      return (
        document.fullscreenElement ||
        document.webkitFullscreenElement ||
        document.mozFullScreenElement ||
        document.msFullscreenElement ||
        null
      );
    } catch (e) {
      return null;
    }
  }

  function isFullscreenSurface(el) {
    var fs = fullscreenElement();
    if (!fs || !el) return false;
    if (fs === el) return true;
    return typeof fs.contains === "function" && fs.contains(el);
  }

  function containsNode(outer, inner) {
    if (!outer || !inner || outer === inner) return false;
    if (typeof outer.contains !== "function") return false;
    try {
      return outer.contains(inner);
    } catch (e) {
      return false;
    }
  }

  function rectCovers(outer, inner) {
    if (!outer || !inner) return false;
    var slack = 8;
    return (
      outer.left <= inner.left + slack &&
      outer.top <= inner.top + slack &&
      outer.right >= inner.right - slack &&
      outer.bottom >= inner.bottom - slack
    );
  }

  // ------------------------------------------------------- DOM bookkeeping

  function remember(acc, el) {
    var i;
    for (i = 0; i < acc.styles.length; i++) {
      if (acc.styles[i].el === el) return acc.styles[i];
    }
    var entry = { el: el, had: null, hadRead: false };
    acc.styles.push(entry);
    return entry;
  }

  function applyInline(acc, el, props) {
    if (!el || !el.style || typeof el.style.setProperty !== "function") return false;
    var entry = remember(acc, el);
    if (!entry.hadRead) {
      // Save the attribute itself: restoring the exact previous text beats
      // guessing which properties the site had set.
      entry.had = el.getAttribute("style");
      entry.hadRead = true;
    }
    var i;
    for (i = 0; i < props.length; i += 2) {
      try {
        el.style.setProperty(props[i], props[i + 1], "important");
      } catch (e) {
        // A page can freeze style objects; one lost property must not abort
        // the whole preparation.
      }
    }
    return true;
  }

  function addClass(acc, el, name) {
    if (!el || !el.classList || typeof el.classList.add !== "function") return false;
    var added = false;
    try {
      if (!el.classList.contains(name)) added = true;
      el.classList.add(name);
    } catch (e) {
      return false;
    }
    if (added) acc.classes.push({ el: el, name: name });
    return true;
  }

  function siblingsOf(el) {
    var parent = el && el.parentNode;
    if (!parent || !parent.children) return [];
    return toArray(parent.children).filter(function (child) {
      return child !== el;
    });
  }

  // ------------------------------------------------------------ candidates

  function surfaceMetrics(el, kind) {
    var rect = rectOf(el);
    if (!rect || !(rect.width > 0) || !(rect.height > 0)) return null;
    if (isHiddenForView(el)) return null;
    var vp = viewportSize();
    var viewportArea = Math.max(1, vp.width * vp.height);
    // Only the part inside the viewport is visible in the PiP window.
    var left = Math.max(0, rect.left);
    var top = Math.max(0, rect.top);
    var right = Math.min(vp.width, rect.right);
    var bottom = Math.min(vp.height, rect.bottom);
    var visibleArea = Math.max(0, right - left) * Math.max(0, bottom - top);
    if (!(visibleArea > 0)) return null;
    return {
      el: el,
      kind: kind,
      rect: rect,
      visibleArea: visibleArea,
      viewportArea: viewportArea,
      coverage: Math.min(1, visibleArea / viewportArea),
      score: 0,
      fullscreen: false,
    };
  }

  function isViable(m) {
    return m.visibleArea >= MIN_VISIBLE_AREA || m.coverage >= MIN_COVERAGE;
  }

  function videoMetrics(el) {
    var m = surfaceMetrics(el, "video");
    if (!m) return null;
    var readyState = numberOrZero(el.readyState);
    var currentTime = numberOrZero(el.currentTime);
    m.ready = readyState >= 2;
    m.playing = el.paused === false && el.ended !== true;
    m.advanced = currentTime > 0;
    m.intrinsicWidth = numberOrZero(el.videoWidth);
    m.intrinsicHeight = numberOrZero(el.videoHeight);
    m.fullscreen = isFullscreenSurface(el);
    return m;
  }

  function iframeMetrics(el) {
    var m = surfaceMetrics(el, "iframe");
    if (!m) return null;
    m.fullscreen = isFullscreenSurface(el);
    m.score = scoreIframe(m, el);
    return m;
  }

  /**
   * Playback state first, geometry second — but never one of them alone:
   * a 3.0 playback weight is what makes a playing clip beat a paused one of
   * the same size, while the size terms (coverage and absolute area) are what
   * keep a small autoplaying ad from beating a large main video.
   */
  function scoreVideo(m, largestVideo) {
    var score = 0;
    if (m.playing) score += 3;
    if (m.ready) score += 1;
    if (m.advanced) score += 0.5;
    score += m.coverage * 2;
    score += Math.min(1, m.visibleArea / (m.viewportArea * 0.25));
    if (m.fullscreen) score += 4;
    if (!m.fullscreen && m.coverage >= NEAR_FULL_COVERAGE && m.visibleArea >= largestVideo) {
      score += NEAR_FULL_BONUS;
    }
    score -= smallnessPenalty(m, largestVideo);
    return score;
  }

  /**
   * Weights every criterion above, but caps how far playback state can carry a
   * clip that is a fraction of the size of the page's largest video: that is
   * what stops a 320x50 autoplaying banner from being selected as "the playing
   * video".
   */
  function smallnessPenalty(m, largestVideo) {
    if (!(largestVideo > 0)) return 0;
    var share = m.visibleArea / largestVideo;
    if (share < TINY_SHARE) return TINY_SHARE_PENALTY;
    if (share < SMALL_SHARE) return SMALL_SHARE_PENALTY;
    return 0;
  }

  /**
   * An iframe carries no playback state we may read, so it is scored as a
   * surface plus the generic signals that say "player": the `allow` attribute,
   * the shape of `src`, and fullscreen.
   */
  function scoreIframe(m, el) {
    var score = m.coverage * 2;
    score += Math.min(1, m.visibleArea / (m.viewportArea * 0.25));
    var allow = attributeOf(el, "allow").toLowerCase();
    if (allow.indexOf("fullscreen") >= 0) score += 0.4;
    if (allow.indexOf("autoplay") >= 0) score += 0.3;
    if (allow.indexOf("picture-in-picture") >= 0) score += 0.2;
    if (allow.indexOf("encrypted-media") >= 0) score += 0.1;
    var src = attributeOf(el, "src").toLowerCase();
    if (
      src.indexOf("player") >= 0 ||
      src.indexOf("embed") >= 0 ||
      src.indexOf("video") >= 0 ||
      src.indexOf("iframe") >= 0
    ) {
      score += 0.35;
    }
    if (
      src.indexOf("doubleclick") >= 0 ||
      src.indexOf("googlesyndication") >= 0 ||
      src.indexOf("adservice") >= 0
    ) {
      score -= 1.5;
    }
    if (m.fullscreen) score += 2;
    return score;
  }

  function attributeOf(el, name) {
    if (!el || typeof el.getAttribute !== "function") return "";
    try {
      return String(el.getAttribute(name) || "");
    } catch (e) {
      return "";
    }
  }

  function bestOf(list) {
    var best = null;
    var i;
    for (i = 0; i < list.length; i++) {
      var candidate = list[i];
      if (!best) {
        best = candidate;
        continue;
      }
      // Deterministic: score, then the larger visible area, then DOM order
      // (the first of two equals wins).
      if (candidate.score > best.score) best = candidate;
      else if (candidate.score === best.score && candidate.visibleArea > best.visibleArea) {
        best = candidate;
      }
    }
    return best;
  }

  function collectCandidates() {
    var videos = [];
    var frames = [];
    var i, m;
    var largestVideo = 0;
    var videoList = toArray(document.querySelectorAll("video"));
    for (i = 0; i < videoList.length; i++) {
      m = videoMetrics(videoList[i]);
      if (!m || !isViable(m)) continue;
      if (m.visibleArea > largestVideo) largestVideo = m.visibleArea;
      videos.push(m);
    }
    // Scored in a second pass: "this clip is much smaller than the biggest
    // video on the page" only becomes knowable once every video was measured.
    for (i = 0; i < videos.length; i++) {
      videos[i].score = scoreVideo(videos[i], largestVideo);
    }
    var frameList = toArray(document.querySelectorAll("iframe"));
    for (i = 0; i < frameList.length; i++) {
      m = iframeMetrics(frameList[i]);
      if (m && isViable(m)) frames.push(m);
    }
    return { videos: videos, frames: frames };
  }

  /**
   * A playing, visible `<video>` wins; a large, player-shaped iframe beats a
   * small one; and a frame that owns the screen beats a small clip that only
   * looks like it is playing.
   */
  function chooseCandidate(videos, frames) {
    var video = bestOf(videos);
    var frame = bestOf(frames);
    if (!video) return frame;
    if (!frame) return video;
    if (video.playing && video.coverage >= 0.15) return video;
    if (frame.coverage >= 1.6 * video.coverage) return frame;
    return video.score >= frame.score ? video : frame;
  }

  // ---------------------------------------------------------------- ratios

  function gcd(a, b) {
    var x = Math.abs(a);
    var y = Math.abs(b);
    while (y) {
      var t = x % y;
      x = y;
      y = t;
    }
    return x || 1;
  }

  /**
   * Integer aspect ratio for `PictureInPictureParams`, or null when the
   * source size is unusable. Android only accepts roughly 1:2.39 .. 2.39:1,
   * so anything outside that range is refused here and left to the caller's
   * fallback instead of being sent as a request the platform silently drops.
   */
  function integerRatio(width, height) {
    if (!(width > 0) || !(height > 0)) return null;
    var ratio = width / height;
    if (!isFinite(ratio) || ratio < 0.42 || ratio > 2.39) return null;
    var i;
    for (i = 0; i < COMMON_RATIOS.length; i++) {
      var pair = COMMON_RATIOS[i];
      if (Math.abs(ratio - pair[0] / pair[1]) < 0.01) {
        return { num: pair[0], den: pair[1] };
      }
    }
    var num = Math.round(ratio * 1000);
    var den = 1000;
    var divisor = gcd(num, den);
    num = Math.floor(num / divisor);
    den = Math.floor(den / divisor);
    if (num < 1) num = 1;
    if (den < 1) den = 1;
    return { num: num, den: den };
  }

  function ratioForCandidate(candidate) {
    if (candidate.kind === "video") {
      var fromIntrinsic = integerRatio(candidate.intrinsicWidth, candidate.intrinsicHeight);
      if (fromIntrinsic) return fromIntrinsic;
    }
    var fromRect = integerRatio(candidate.rect.width, candidate.rect.height);
    if (fromRect) return fromRect;
    return { num: FALLBACK_NUM, den: FALLBACK_DEN };
  }

  /**
   * `sourceRectHint` wants window coordinates in device pixels; the rect we
   * measured is in CSS pixels, so it is scaled by the device pixel ratio. The
   * Activity's WebView fills the window, so the viewport origin maps 1:1.
   */
  function deviceRect(rect) {
    var dpr = numberOrZero(window.devicePixelRatio) || 1;
    return {
      left: Math.max(0, Math.round(rect.left * dpr)),
      top: Math.max(0, Math.round(rect.top * dpr)),
      width: Math.max(0, Math.round(rect.width * dpr)),
      height: Math.max(0, Math.round(rect.height * dpr)),
    };
  }

  // -------------------------------------------------------- applying/undo

  /**
   * The stage is the surface that gets lifted to cover the PiP window: the
   * fullscreen element when there is one, else the tightest wrapper around
   * the player (so its own controls come along), else the player itself.
   * A page-sized ancestor is never used: lifting it would drag unrelated page
   * content into the PiP window.
   */
  function chooseStage(target, targetRect) {
    var fs = fullscreenElement();
    if (fs && (fs === target || containsNode(fs, target))) return fs;
    var parent = target.parentNode;
    if (!parent || parent === document.body || isDetached(parent)) return target;
    var parentRect = rectOf(parent);
    if (!parentRect) return target;
    if (!rectCovers(parentRect, targetRect)) return target;
    var parentArea = areaOf(parentRect);
    var targetArea = areaOf(targetRect);
    if (targetArea > 0 && parentArea > targetArea * STAGE_MAX_GROWTH) return target;
    return parent;
  }

  function liftChain(stage) {
    var chain = [];
    var node = stage;
    var depth = 0;
    while (node && node !== document.documentElement && depth < LIFT_DEPTH) {
      chain.push(node);
      if (node === document.body) break;
      node = node.parentNode;
      depth++;
    }
    return chain;
  }

  function applyView(candidate) {
    var acc = newAccumulator();
    var target = candidate.el;
    var root = document.documentElement;
    var body = document.body;
    if (!root || !body) return null;

    var stage = chooseStage(target, candidate.rect);
    var chain = liftChain(stage);

    // 1. The player fills the surface. Applied before the stage style so a
    //    bare player (stage === target) ends up fixed, not relative.
    applyInline(acc, target, TARGET_STYLE);
    addClass(acc, target, "animehub-pip-target");
    // 2. The stage covers the viewport; its ancestors must stop clipping or
    //    containing the fixed box (transform/filter/contain/will-change).
    applyInline(acc, stage, STAGE_STYLE);
    addClass(acc, stage, "animehub-pip-stage");
    applyInline(acc, root, ROOT_STYLE);
    applyInline(acc, body, ROOT_STYLE);
    var i;
    for (i = 1; i < chain.length; i++) {
      applyInline(acc, chain[i], ANCESTOR_STYLE);
      addClass(acc, chain[i], "animehub-pip-lift");
    }
    // 3. Everything beside the chain — headers, comments, ad slots, popups —
    //    stops painting. `visibility` is used instead of `display`/`remove` so
    //    no layout or framework state changes: hidden subtrees keep their
    //    boxes, their scroll offsets and their listeners.
    for (i = 0; i < chain.length; i++) {
      var siblings = siblingsOf(chain[i]);
      for (var j = 0; j < siblings.length; j++) {
        applyInline(acc, siblings[j], SIBLING_STYLE);
      }
    }
    // 4. Inside the stage, an in-flow sibling ABOVE the video (a site header)
    //    would push the video down, since the video is sized to the stage.
    //    Siblings below it are clipped by the stage's overflow instead, and
    //    overlay controls overlap the video, so both stay. Only siblings that
    //    end above the video's top edge and do not overlap it are hidden.
    var targetRect = candidate.rect;
    var node = target;
    while (node && node !== stage && node !== body && node !== root) {
      var beside = siblingsOf(node);
      for (var k = 0; k < beside.length; k++) {
        var sib = rectOf(beside[k]);
        if (sib && sib.height > 0 && sib.bottom <= targetRect.top + 1 && !overlapsRect(sib, targetRect)) {
          applyInline(acc, beside[k], SIBLING_STYLE);
        }
      }
      node = node.parentNode;
    }
    return { touched: acc.styles, classes: acc.classes };
  }

  function restoreState(applied) {
    var i;
    for (i = 0; i < applied.classes.length; i++) {
      try {
        applied.classes[i].el.classList.remove(applied.classes[i].name);
      } catch (e) {
        // Detached or re-rendered node: nothing to clean up.
      }
    }
    for (i = 0; i < applied.touched.length; i++) {
      var entry = applied.touched[i];
      try {
        if (entry.had === null || entry.had === undefined) {
          entry.el.removeAttribute("style");
        } else {
          entry.el.setAttribute("style", entry.had);
        }
      } catch (e) {
        // Same: a SPA may have replaced the node while PiP was open.
      }
    }
  }

  // --------------------------------------------------------------- API

  function fail(reason) {
    return { ok: false, reason: reason };
  }

  function release() {
    if (!state) return false;
    var applied = state.applied;
    state = null;
    restoreState(applied);
    return true;
  }

  function prepare() {
    try {
      // Re-entrant by design: a second request (or a retry after a size
      // change) starts from the page's real state, never from a stacked one.
      finishProbe();
      release();
      if (!document.body || !document.documentElement) return fail("no-body");

      var found = collectCandidates();
      var candidate = chooseCandidate(found.videos, found.frames);
      if (!candidate) return fail("no-candidate");

      var applied = applyView(candidate);
      if (!applied) return fail("apply-failed");

      var ratio = ratioForCandidate(candidate);
      state = { applied: applied, candidate: candidate, wasPlaying: wasPlaying(candidate) };
      lastResume = "none";
      probe = startProbe(candidate);
      return {
        ok: true,
        kind: candidate.kind,
        fullscreen: !!candidate.fullscreen,
        num: ratio.num,
        den: ratio.den,
        rect: deviceRect(candidate.rect),
        score: Math.round(candidate.score * 100) / 100,
      };
    } catch (e) {
      // A page that throws mid-preparation must not stay half-styled.
      try {
        release();
      } catch (inner) {
        // ignored: the release is best-effort
      }
      return fail("exception");
    }
  }

  // A video that was playing before PiP is paused by the WebView when the
  // Activity pauses (WebView.onPause stops media); it is started again once
  // the WebView is resumed in PiP. Only a real <video> is touched.
  function wasPlaying(candidate) {
    var el = candidate && candidate.el;
    return !!el && el.tagName === "VIDEO" && el.paused === false && el.ended !== true;
  }

  function resumePlayback(el, shouldPlay) {
    if (!shouldPlay || !el || el.tagName !== "VIDEO" || el.paused !== true || typeof el.play !== "function") {
      lastResume = "skipped";
      return lastResume;
    }
    try {
      var started = el.play();
      // Set before the handler is attached: a rejection, asynchronous in a
      // browser, always overwrites this value.
      lastResume = "called";
      if (started && typeof started.catch === "function") {
        started.catch(function (err) {
          lastResume = "rejected:" + errorName(err);
        });
      }
    } catch (e) {
      lastResume = "threw:" + errorName(e);
    }
    return lastResume;
  }

  // ------------------------------------------------------ diagnostics probe
  // Read-only observation of the chosen player while PiP is up. It only adds
  // event listeners and reads state; it never changes the page. Listeners are
  // attached when the view is applied and removed when PiP is left.

  var probe = null;      // the live probe while a view is applied
  var lastReport = "no-report";
  var lastResume = "none";

  function errorName(err) {
    return err && typeof err.name === "string" ? err.name.slice(0, 40) : "unknown";
  }

  function startProbe(candidate) {
    var el = candidate.el;
    var p = {
      el: el,
      isVideo: el.tagName === "VIDEO",
      t0: Date.now(),
      events: [],
      handlers: [],
    };
    function note(name) {
      if (p.events.length < 40) p.events.push(name + "@" + (Date.now() - p.t0));
    }
    function listen(target, name) {
      if (!target || typeof target.addEventListener !== "function") return;
      var handler = function () {
        note(name);
      };
      target.addEventListener(name, handler);
      p.handlers.push([target, name, handler]);
    }
    if (p.isVideo) {
      listen(el, "pause");
      listen(el, "play");
      listen(el, "ended");
      listen(el, "waiting");
      listen(el, "stalled");
    }
    listen(document, "visibilitychange");
    listen(window, "blur");
    listen(window, "focus");
    return p;
  }

  function buildReport(p) {
    var el = p.el;
    var parts = [];
    parts.push("kind=" + (p.isVideo ? "video" : "iframe"));
    parts.push("ms=" + (Date.now() - p.t0));
    if (p.isVideo) {
      parts.push("paused=" + (el.paused === true ? 1 : 0));
      parts.push("ended=" + (el.ended === true ? 1 : 0));
      parts.push("readyState=" + numberOrZero(el.readyState));
    }
    parts.push("visibility=" + String(document.visibilityState || "na"));
    var focused = typeof document.hasFocus === "function" ? document.hasFocus() : null;
    parts.push("focus=" + (focused === null ? "na" : focused ? 1 : 0));
    parts.push("resume=" + lastResume);
    parts.push("events=" + (p.events.length ? p.events.join(",") : "none"));
    return parts.join(";");
  }

  function finishProbe() {
    if (!probe) return;
    lastReport = buildReport(probe);
    for (var i = 0; i < probe.handlers.length; i++) {
      var h = probe.handlers[i];
      h[0].removeEventListener(h[1], h[2]);
    }
    probe = null;
  }

  function reapply() {
    if (!state) return false;
    var candidate = state.candidate;
    if (!candidate || !candidate.el || isDetached(candidate.el)) {
      release();
      return false;
    }
    var shouldPlay = state.wasPlaying;
    release();
    var applied = applyView(candidate);
    if (!applied) return false;
    // The resume is one-shot: a later re-assert (the Activity pausing again,
    // or the PiP mode change) must never start a player the user paused.
    state = { applied: applied, candidate: candidate, wasPlaying: false };
    resumePlayback(candidate.el, shouldPlay);
    return true;
  }

  /**
   * `__animehubPip(false)` restores, `__animehubPip(true)` re-asserts, and a
   * bare call reports whether a PiP view is currently applied.
   */
  function report() {
    return probe ? buildReport(probe) : lastReport;
  }

  function toggle(active) {
    if (active === false) {
      finishProbe();
      return release();
    }
    if (active === true) return reapply();
    return state ? true : false;
  }

  if (window[VERSION_KEY] === VERSION && typeof window[PREPARE_FN] === "function") {
    // A copy of this exact version is already installed: leave it (and any
    // PiP view it is holding) alone.
    return;
  }

  window[VERSION_KEY] = VERSION;
  window[PREPARE_FN] = prepare;
  window[TOGGLE_FN] = toggle;
  window[REPORT_FN] = report;
})();
