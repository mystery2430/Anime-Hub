// Popup guard, injected by `build_init_script` (web/session.rs) into every
// site WebView frame when `InjectedConfig.block_popups` is set.
//
// WHY IT RUNS IN EVERY FRAME: with the Android site WebView configured
// `supportMultipleWindows=false` and no `onCreateWindow` handler, a script
// `window.open` does not open a window — it becomes an ordinary SAME-VIEW
// navigation. That is what makes it worth stopping: a third-party ad frame
// would otherwise navigate the visible page away from the player (and on
// desktop produce the blank-tab/spinner artefact). Ad frames are exactly the
// callers, so scoping this to the top frame would keep the guard where it is
// useless and drop it where it is needed. Wherever a navigation does get
// through, the native policy (`SiteNavigationGuard` / `SiteNavigationPolicy`,
// and the Rust navigation handlers on desktop) still decides it.
//
// WHAT IT DELIBERATELY DOES NOT DO: keep a reference to the native
// `window.open`. An earlier version stored the original under a
// non-enumerable property of the replacement, "for first-party code that
// feature-detects". Nothing in this app ever read it — and because the
// document-start channel applies to every origin, it handed a working bypass
// of this guard to every cross-origin frame a site embeds: an ad iframe could
// reach the stored original and navigate the view after all. The replacement
// below is therefore the only `window.open` a frame can see, and it captures
// nothing: no stored original, no extra own property, no prototype change.
window.open = function () {
  return null;
};
