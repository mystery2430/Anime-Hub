# Android Picture-in-Picture — implementation and verification report

Status: **implemented; the WebView side is unit-tested; the Android half
compiles and packages into a debug APK in CI (PR #19, all jobs green); not
device-tested.** See [Verification](#verification) for exactly what was and was
not run.

AnimeHub opens every site in one shared Android WebView. Android PiP shrinks
the **whole Activity**, so the PiP window would show the entire site — header,
episode list, comments, ad slots — instead of the video. This document
describes the controlled chain that fixes that: the WebView is prepared into a
player-only view **first**, and PiP is entered **after** the page confirms it.

---

## 1. Changed files

### New

| File | What it is |
|---|---|
| `src-tauri/src/web/pip_controller.js` | The WebView-side controller. Finds the active player, applies the PiP-only view, restores it byte-exactly, and exposes `window.__animehubPreparePip()` / `window.__animehubPip(...)`. Self-contained IIFE; no IPC, no dependencies. |
| `src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPipController.kt` | Kotlin **template** (`object AnimeHubPipController`) for the native half: WebView binding, the prepare → callback → enter chain, `sourceRectHint`, aspect clamping, restore, and the auto-enter preference. `scripts/android_prepare.py` fills its `__ANIMEHUB_PIP_CONTROLLER_JS__` placeholder and copies it into the generated app package. |
| `tests/pip_controller.test.js` | 25 tests for the controller: 24 behavioural ones against a fake DOM (`node:vm`), plus a source-level one (no `$`/`"""`, no iframe internals, no HTML sinks, no network). |
| `tests/helpers/fake_dom.js` | The minimal fake DOM those tests run against (nothing beyond the surface the controller touches). |
| `docs/android-pip.md` | This document. |

### Modified

| File | Change |
|---|---|
| `scripts/android_prepare.py` | Renders `AnimeHubPipController.kt` from the template with the same JS Rust ships, and injects/refreshes the marker-delimited PiP lifecycle block (plus the three imports it needs) into the generated `MainActivity.kt`. Refuses to inject into a `MainActivity` that does not extend `TauriActivity`, and refuses to embed JS containing `$` or `"""`. |
| `src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt` | `enter_pip` and `set_pip_auto_enter` now delegate to `AnimeHubPipController` instead of building `PictureInPictureParams` themselves. The duplicated PiP code and its imports are gone; the plugin no longer calls `enterPictureInPictureMode` at all. |
| `src-tauri/src/web/session.rs` | `InjectedConfig` gains `pip_controller`; when set, `build_init_script` appends the controller (via `include_str!`, so the injected text is the same file the Kotlin embeds). Three new tests pin that behaviour and the embedding rules. |
| `src-tauri/src/commands.rs` | `open_site` sets `pip_controller: cfg!(target_os = "android")` — desktop keeps the smaller script. Documentation on `enter_pip` / `set_pip_auto_enter` now describes the real semantics. |
| `src-tauri/src/android_bridge.rs` | Doc comments only (the wrappers already existed and keep their signatures). |
| `src-tauri/android-plugin/src/lib.rs` | Doc comments + two new tests: the controller template contains the full API and never enables platform auto-enter; the plugin delegates instead of duplicating. |
| `src-tauri/src/lib.rs` | On Android, the stored `pip_auto_enter` preference is re-applied to the native controller at startup on a short-lived thread with a bounded retry, so app startup is never blocked. |
| `tests/gaps.test.js` | Fake gen tree now looks like the real generated project; asserts the controller rendering, the lifecycle injection, idempotence (no duplicate `onDestroy`), and the wrong-base-class refusal. |
| `README.md`, `CHANGELOG.md`, `HANDOFF.md`, `index.html` | User-facing wording, changelog entry, and the honest validation status. |
| `package.json` | `npm test` now includes `tests/pip_controller.test.js`. |

Not touched: site URLs, site registry, cookie isolation, storage system,
provider IDs, navigation security, blocklist, desktop WebView logic, the
frontend framework. There is no TypeScript in this change and no new
dependency (no Media3/ExoPlayer).

---

## 2. PiP entry lifecycle

```
 PiP request
 ├─ API 30+  MainActivity.onPictureInPictureRequested()  -> returns true (claims it)
 ├─ API 26-29 MainActivity.onUserLeaveHint()
 └─ app call  enter_pip command -> AnimeHubPlugin.enter_pip
        |
        v
 AnimeHubPipController.requestEnter(activity, trigger)
   - isSupported()?  (API 26+, FEATURE_PICTURE_IN_PICTURE)
   - already in PiP, no WebView, or a chain in flight -> answer false, stop
        |
        v  (main thread)
 WebView.evaluateJavascript(prepareScript())
   - installs src-tauri/src/web/pip_controller.js if this document lacks it
   - calls window.__animehubPreparePip()
        |
        v  JS finds the active player, applies the player-only view,
           answers { ok, kind, num, den, rect }
        |
        v  (main thread, inside the evaluateJavascript callback)
 JSON parse -> PictureInPictureParams(ratio + sourceRectHint)
             -> Activity.enterPictureInPictureMode(...)
        |
   on failure (no player / unreadable answer / not resumed) -> restore() and
   report false: the page is left exactly as it was, no PiP window appears.
```

The entry and the exit are **separate callbacks** by design:

```kotlin
override fun onPictureInPictureModeChanged(isInPictureInPictureMode: Boolean, newConfig: Configuration) {
  super.onPictureInPictureModeChanged(isInPictureInPictureMode, newConfig)
  if (!isInPictureInPictureMode) {
    AnimeHubPipController.restore(AnimeHubPipController.webView())
  }
}
```

Entering is never done from `onPictureInPictureModeChanged(true, …)`: that
callback fires *after* the system has already shrunk the window, which is too
late to prepare the page. `super` is called first so the Activity picks up the
new `Configuration` before the page is restored.

### API-level paths

| API | Entry point | Why |
|---|---|---|
| 26-29 (`Android 8.0-10`) | `onUserLeaveHint()` | `onPictureInPictureRequested()` does not exist yet. `onUserLeaveHint` is exactly "the user left the app", documented for PiP entry. |
| 30+ (`Android 11+`) | `onPictureInPictureRequested()`, returns `true` | AOSP `Activity.java`: *"@return true if the activity received this callback regardless of if it acts on it or not. If false, the framework will assume the app hasn't been updated to leverage this callback and will in turn send a legacy callback of onUserLeaveHint()"* (`android-11.0.0_r1`). Claiming it stops the framework's own pause → `onUserLeaveHint` → resume dance; the app keeps full control. |
| 26-29 | `onUserLeaveHint()` is ignored on API 30+ | The two paths can never both fire for one gesture. |
| 31+ (`Android 12+`) | same as 30+, plus `setPictureInPictureParams` with `setAutoEnterEnabled(false)` | See §6. |

Preconditions enforced in Kotlin: the device must report
`FEATURE_PICTURE_IN_PICTURE`, and the Activity must still be enterable when the
JS answers. `Activity.enterPictureInPictureMode()` throws
`IllegalStateException("Activity must be resumed to enter picture-in-picture")`
once the Activity is stopped (`mCanEnterPictureInPicture = false` in
`performStop`) — that is caught, logged, the page is restored, and the caller
gets `false`. A slow WebView can therefore lose the race with the stop (see
[Known limits](#9-known-limits)); the consequence is "no PiP window", never a
crash, a stranded page, or a lost session.

### The `enter_pip` command

`enter_pip` has no frontend caller today, but it is registered and reachable, so
it goes through **the same** `requestEnter` chain (trigger `"command"`) with the
requested ratio as a fallback. `AnimeHubPlugin.enter_pip` resolves
`{ ok: entered }` exactly once — including when the controller cannot start
(unsupported device, no WebView, chain already running), so an `invoke` can
never hang.

---

## 3. Active-player selection

`collectCandidates()` measures every `document.querySelectorAll("video")` and
`("iframe")` element; nothing about the decision is site-specific (no class, id
or DOM path from any known site is used anywhere in the controller).

A candidate must be present in the viewport to be considered at all:

* `getBoundingClientRect()` has `width > 0` and `height > 0`,
* the element is not hidden (`checkVisibility`/computed style/ancestors),
* the part of the rect **inside the viewport** (`visibleArea`) is meaningful:
  `≥ 12000 px²` or `≥ 1.5 %` of the viewport,
* for videos: `readyState >= 2` / `!paused` / `currentTime > 0` are *read* and
  weighted, never required one by one.

Scoring, in the order the task ranks the signals:

| Signal | Weight |
|---|---|
| `paused === false && !ended` | +3 |
| `readyState >= 2` | +1 |
| `currentTime > 0` | +0.5 |
| coverage (`visibleArea / viewportArea`) | +`coverage * 2` |
| absolute size (capped at a quarter of the viewport) | +`min(1, area / (viewport * 0.25))` |
| `document.fullscreenElement` is the video or contains it | +4 |
| fills ≥ 70 % of the viewport **and** is the largest video on the page | +3 ("fullscreen without the Fullscreen API") |
| much smaller than the page's largest video | −3.5 (< 25 %) / −1.0 (< 60 %) |

The smallness penalty is what keeps a small autoplaying ad from winning on
playback state alone, and it is why the ranking is a blend rather than a single
condition: a playing clip beats an equally sized paused one, but a 320×50
banner never outranks a 360×203 main video, playing or not.

Ties are broken deterministically: score, then larger visible area, then DOM
order.

**Fullscreen** is detected three ways — `document.fullscreenElement`
(`webkitFullscreenElement` as fallback) being the video, a wrapper containing
the video, or the near-full-viewport rule above — so nothing depends on a site
calling `requestFullscreen()`.

`chooseCandidate()` decides between the best video and the best iframe:

1. a playing video covering ≥ 15 % of the viewport wins,
2. a frame covering ≥ 1.6× the video's area wins (a player *is* the frame),
3. otherwise the higher score wins.

---

## 4. Cross-origin iframes

Some providers play inside an `<iframe>` (their own embed player). The
controller never inspects frame internals:

* it never touches `contentDocument` / `contentWindow.document`, and does not
  special-case same-origin frames either — one fewer branch, one fewer way to
  break a site;
* it never navigates, replaces, or reloads a frame;
* the frame is scored as a **surface**: visibility and area (as above, same
  geometric rules), its `allow` attribute (`fullscreen`, `autoplay`,
  `picture-in-picture`, `encrypted-media` add weight), its `src` shape
  (`player`/`embed`/`video`/`iframe` add weight; `doubleclick`,
  `googlesyndication`, `adservice` subtract 1.5), and fullscreen state;
* on selection, the frame itself is lifted to fill the PiP viewport
  (`width`/`height` 100 %, its own aspect ratio preserved by the box, not by
  touching what is inside it).

Preference order is the one the task requires: a playing visible `<video>`
first, then a large player-shaped frame, then anything else.

---

## 5. The PiP view and the restore

The PiP view is pure inline CSS plus three disposable classes. Nothing is
created, moved or removed in the DOM — `remove()`, `appendChild()`,
`replaceChildren()` and `insertBefore()` do not appear in the controller, and a
test asserts that no element is added or removed and that the video keeps its
parent and playback state.

What is applied, and what the restore guarantees:

| Applied | Recorded for the restore |
|---|---|
| `html`/`body`: `margin: 0`, `background: #000`, `overflow: hidden` | the exact previous inline `style` **attribute text** of each element touched |
| Stage (the fullscreen element, else the tightest wrapper around the player that is at most 4× its area, else the player): `position: fixed; inset: 0` and the ancestor properties that would trap a fixed box | idem; ancestors up to a depth of 8 get the same treatment |
| Player: fills the stage, `object-fit: contain` (video) — aspect ratio preserved, never cropped, never stretched | idem |
| Siblings of every element in the lifted chain: `visibility: hidden` | idem |
| Two classes: `animehub-pip-stage`, `animehub-pip-target` (and `animehub-pip-lift` on ancestors) | the classes are removed again — only classes that were actually added are touched, so a site class that happens to have our name survives the round trip |

`visibility: hidden` is used instead of `display: none` or removal on purpose:
hidden subtrees keep their layout boxes, their scroll offsets, their listeners
and their framework state, which is what makes the exit lossless for SPA
players. The player's own controls are inside the lifted chain and stay
visible.

`window.__animehubPip(false)` (called from the Kotlin exit callback) walks the
recorded list and writes the saved attribute text back — elements that had no
inline `style` get the attribute removed, not set to empty — then removes the
added classes. Nothing else is touched, so playback is never paused, no
navigation happens and no reload is triggered. If a SPA re-rendered and a
recorded node is detached, it is skipped safely.

`prepare()` is re-entrant: a second call (or a retry after a rotation inside
PiP) first restores the page to its real state, so views never stack.
`window.__animehubPip(true)` re-asserts the current view after a resize without
losing the selection.

---

## 6. Why platform auto-enter stays off

The setting's label promises "PiP when the video is playing and the user goes
home". The obvious implementation — `setAutoEnterEnabled(true)` — cannot keep
that promise:

* AOSP, `PictureInPictureParams.Builder#setAutoEnterEnabled` (`android-12.0.0_r1`):
  *"Sets whether the system will automatically put the activity in
  picture-in-picture mode without needing/waiting for the activity to call
  `Activity#enterPictureInPictureMode(PictureInPictureParams)`. **If true,
  `Activity#onPictureInPictureRequested()` will never be called.**"*
* The server implements exactly that: `ActivityClientController` enters PiP
  itself when `r.pictureInPictureArgs.isAutoEnterEnabled()` (line ~768) and
  otherwise schedules the client callback; `ActivityRecord.makeInvisible()`
  enters PiP directly for auto-enter-enabled activities instead of giving the
  app a turn.

So with auto-enter on, the system would put the unprepared, full-page WebView
into the PiP window and the player-only view could never be applied. The app
therefore always sets `setAutoEnterEnabled(false)` (API 31+ guarded) and the
user preference instead **arms the controlled paths**:

* `autoEnterEnabled` gates `onPictureInPictureRequested()` (API 30+) and
  `onUserLeaveHint()` (API 26-29);
* with the setting **off**, `onPictureInPictureRequested()` still returns
  `true` (the contract is "the app received the callback") but does nothing, so
  no PiP window appears and the framework does not fall back to its legacy
  pause/`onUserLeaveHint`/resume cycle — which would otherwise reach the code
  path we deliberately closed;
* with the setting **on**, a gesture that would background the app runs the
  full prepare chain; no player → no PiP → the site stays untouched, which is
  the "never enter PiP unprepared" rule.

Because the setting only lives in Rust's settings store, `src-tauri/src/lib.rs`
re-applies it to the native controller at startup: a short-lived thread calls
`set_pip_auto_enter` and retries up to four times (400 ms apart) if the plugin
bridge is not answering yet, so neither the Activity's main thread nor app
startup is blocked. Without this, a restart would leave the Kotlin object at
its default and silently disable auto-PiP; a failure is logged as a warning and
the settings toggle keeps working.

---

## 7. `sourceRectHint`

The JS reports the selected player's `getBoundingClientRect()` scaled by
`devicePixelRatio`; the Activity's WebView fills the window, so the viewport
origin maps 1:1 to window coordinates. Kotlin clamps the rect to the WebView's
width/height, drops degenerate rects, and passes it as
`PictureInPictureParams.Builder.setSourceRectHint(...)`.

`sourceRectHint` is only a **transition hint** — AOSP: *"These bounds are only
used when an activity first enters picture-in-picture, and describe the bounds
in window coordinates of activity entering picture-in-picture that will be
visible following the transition"* — it does not crop anything. The player-only
look comes entirely from the CSS view; the hint only makes the shrink animation
start from the video instead of from the whole page.

---

## 8. Aspect ratio

* The video's intrinsic `videoWidth`/`videoHeight` when they are usable, else
  the measured rect, else 16:9 (`FALLBACK_NUM`/`FALLBACK_DEN`).
* `integerRatio()` snaps to common ratios (16:9, 4:3, 21:9, 3:2, 1:1 and their
  inverses) inside a tolerance, otherwise rounds to three decimals and reduces
  by gcd — always a positive integer pair.
* Kotlin then clamps to what Android accepts (`MAX_RATIO = 2.39`,
  `MIN_RATIO = 1/2.39`) — a rejected ratio silently disables PiP, so an invalid
  pair never reaches `setAspectRatio`. `android_bridge::clamp_aspect` (0/0 →
  16:9, 1:2.39 … 2.39:1) is untouched and still clamps the command path.

---

## 9. Known limits

* **The stop race.** Between the PiP request and the JS answer the Activity can
  be stopped; `enterPictureInPictureMode` then refuses and no PiP window
  appears. The same test suite and the Rust test in
  `session.rs` reject a controller file containing them. The prepare → callback → enter order never violates the task's rule
  ("never `evaluateJavascript` immediately followed by
  `enterPictureInPictureMode`"), and the callback hop is a single main-thread
  round trip, but the race cannot be removed from Kotlin. Device measurement
  would show how often it is hit in practice.
* **Decorative full-bleed video.** A page whose background video fills the
  viewport *and* is the largest video on the page can outscore the real player.
  Streaming sites do not normally do this; the smallness penalty and the
  largest-video gate keep the common cases right.
* **Player inside a closed shadow root / canvas-only player.** Nothing to
  select; the preparation fails and the page is left alone.
* **Session isolation.** Unchanged by this work: PiP does not add or change any
  storage, cookie or provider behaviour.

---

## 10. Security

* **No new IPC surface.** The controller runs in the site's document and cannot
  reach Tauri: `src-tauri/tauri.conf.json` keeps the `default` capability for
  the main webview only, and remote pages get no IPC access. The annotated
  `enter_pip` / `set_pip_auto_enter` commands are called from AnimeHub's own
  launcher UI, not from a site page.
* **The controller only reads and styles the DOM.** It performs no network
  request, uses no HTML sink (`innerHTML`, `outerHTML`, `insertAdjacentHTML`,
  `document.write`, `eval`, `new Function` are all absent — verified over
  comment-stripped code — and never navigates or reads/writes web storage.
* **No interpolation of page-controlled strings into native code.** The Kotlin
  side reads exactly one JSON object back; it never builds a script from page
  data. The JS is embedded from Rust (`include_str!`) and from
  `scripts/android_prepare.py`, never from a runtime string.
* **No `$` and no `"""` in the JS.** The file is embedded verbatim in a Kotlin
  raw string; both sequences would break the build in confusing ways, so the
  prepare script, the JS test suite and the Rust test in `session.rs` all refuse
  to continue if either appears.

---

## 11. Regenerating the Android project

`src-tauri/gen/android` is generated and ignored; nothing in this change is
hand-edited there.

```bash
npm run tauri android init        # (or: cargo tauri android init)
python3 scripts/android_prepare.py
```

The script is idempotent: the PiP lifecycle block is delimited by

```
// >>> AnimeHub PiP lifecycle (injected by scripts/android_prepare.py) >>>
...
// <<< AnimeHub PiP lifecycle <<<
```

and replaced on every run, so a second run cannot produce a duplicate
`onDestroy` override (which would not compile). The same file is copied into the
generated package for `AnimeHubPlugin.kt` and the rendered
`AnimeHubPipController.kt`. CI (`ci.yml`, `release-android.yml`) runs the script
before `tauri android build`.

---

## 12. Verification

### What was run, and where

| Command | Result |
|---|---|
| `node --test tests/pip_controller.test.js` | **25/25 pass** (24 behavioural + the source-level guarantees) |
| `npm test` (all suites) | **92/92 pass** |
| `node --test tests/gaps.test.js` | **7/7 pass** — includes the prepare-script tests below |
| `python3 scripts/android_prepare.py --gen <fake gen tree>` | Renders the controller, injects the lifecycle block + imports, is idempotent on a second run, and refuses a `MainActivity` that is not a `TauriActivity` |
| `node --check src-tauri/src/web/pip_controller.js` | Syntax OK |
| tree-sitter grammars (`tree-sitter-rust` 0.24, `tree-sitter-kotlin` 1.1, via pip) over every changed `.rs` file and the three generated Kotlin files (controller, plugin, `MainActivity`) | No `ERROR`/missing nodes — a full parse of each file, not just a brace count |

### CI

`ci.yml` runs on pull requests, so opening PR #19 from this branch ran the whole
pipeline — **run [37792307795](https://github.com/mystery2430/Anime-Hub/actions/runs/37792307795)
on commit `c82456d`: every job green**:

| Job | Result |
|---|---|
| **Android compile (aarch64)** — `tauri android init` → `python3 scripts/android_prepare.py` → `tauri android build -- --debug --apk --target aarch64` | **success.** The generated `MainActivity` with the injected lifecycle block, `AnimeHubPipController.kt` and the reworked `AnimeHubPlugin.kt` all compile and package. Artifact `animehub-android-aarch64-debug` (14.7 MB) is a real, installable debug APK. |
| Tests (ubuntu-24.04, macos-latest, windows-latest) — `npm test`, `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --all` | success on all three |
| Dependency and secret audit | success |
| Release profile (LTO) | success |

The first run (commit `9393b35`) failed **only the three test jobs** and its
Android job already passed: two new Rust assertions matched text the shipped
files do not contain (the controller installs its API through computed keys,
`window[PREPARE_FN] = prepare`, and `AnimeHubPlugin.kt` mentions
`enterPictureInPictureMode()` in the doc comment that explains the delegation).
Both assertions were corrected in `c82456d` to test the real source lines and
the call form — that is the commit CI is green on.

The APK artifact could not be downloaded into the authoring sandbox (the
artifact blob host is outside its network allowlist); get it from the run page
above or from the PR's *Checks* tab.
| Brace/paren balance of the rendered `AnimeHubPipController.kt`, `AnimeHubPlugin.kt` and generated `MainActivity.kt` after rendering (script in the session, not committed) | Balanced; no placeholder left behind |

The `gaps` tests additionally pin: the embedded controller text in the
generated Kotlin is byte-identical to `src-tauri/src/web/pip_controller.js`;
the overrides `onWebViewCreate`, `onPictureInPictureRequested`,
`onUserLeaveHint`, `onPictureInPictureModeChanged`, `onDestroy` are present
exactly once each after two runs; and the lifecycle block is never injected into
a non-Tauri Activity.

### What was **not** run — do not read this as "it works on a device"

* **Nothing was compiled or run in the authoring sandbox.** It has no Android
  SDK, NDK, Gradle or JDK and no `src-tauri/gen/android`, and no Rust toolchain.
  Everything in the CI table above ran on GitHub's runners, not here: the
  `android_prepare.py` run against a *real* generated project, the Kotlin
  compile, the APK link, `cargo test`, `cargo clippy`. Locally the script was
  only exercised against a hand-built fake gen tree.
* **No device or emulator run.** The APK exists and installs, but nothing in
  this change has been observed running: the PiP transition, the player
  selection on a real site, the restore, or the API-level differences. The
  checklist below is what still has to be done on hardware.
* **Nothing was tested on a device.** No APK was produced in this workspace
  (see above), so there is no emulator or hardware run behind this document.
* **`cargo test` was not re-run** (no Rust toolchain here). The Rust changes are
  additive: one new `InjectedConfig` field (existing tests updated), one
  `#[cfg(target_os = "android")]` startup block, doc comments and new tests.
* **The `sourceRectHint` transition animation and the PiP window's real
  behaviour on 26-29 vs 30+ vs 31+ were not observed.** All API-level claims
  above come from AOSP source, and are quoted with the file and tag they were
  read from.

### Device checklist (for the first real build)

1. No video on the page → home gesture → no PiP, page unchanged, no crash.
2. One playing video → home → PiP shows only the player (no header/episode
   list), ratio matches the video.
3. Two videos (main playing, second paused) → main one is shown.
4. Small autoplaying ad present → the ad is never picked.
5. Cross-origin iframe player → the frame fills the PiP window.
6. Enter PiP → the PiP window shows the player, not the site.
7. Exit PiP → the page is pixel-identical to before (scroll position, header,
   player controls) and nothing reloaded.
8. Audio/video keeps playing through entry and exit, `currentTime` continues.
9. Fullscreen video → PiP → exit → the site's own fullscreen state returns.
10. Force a SPA episode switch / player re-render while PiP is open → exiting
    must not throw and must not leave hidden sections.
11. API 26-29 device: the `onUserLeaveHint` path is the one that runs.
12. API 30 device: the `onPictureInPictureRequested` path is the one that runs.
13. API 31+ device: confirm the system never enters PiP on its own with the
    setting off, and that the setting on uses the prepared path.
14. Auto-PiP off → home gesture → no unexpected PiP window at any point.
15. Break the preparation (e.g. pause the video and remove it) → no PiP, no
    console crash, page usable.
