/**
 * Theme persistence flow with optimistic UI and deterministic rollback.
 *
 * Kept DOM-agnostic: callers inject `draw` (DOM + buttons + selector) and
 * `cache` (localStorage mirror) so Node tests can drive the logic without a
 * real document.
 *
 * Rules:
 * - normalize to system/dark/light
 * - persist via `update_settings` is the source of truth
 * - optimistic UI is applied immediately, but on persist failure every visible
 *   state (DOM, themePref, localStorage, selector/buttons) is reverted to the
 *   previously persisted value
 * - native window theme is applied only after persist succeeds; its failure is
 *   cosmetic and does not revert the saved preference
 * - rapid successive calls are queued; an older async completion never overwrites
 *   a newer user choice
 */

import { normalizeTheme } from "./theme.js";

export function createThemeFlow(deps) {
  const { call, notify, errorText, getCurrent, setCurrent, draw, cacheSet, applyNative } = deps;

  let seq = 0;
  let chain = Promise.resolve();

  function enqueue(wantedRaw) {
    const id = ++seq;
    const wanted = normalizeTheme(wantedRaw);
    const task = chain.then(() => run(id, wanted));
    // Keep chain alive even if task rejects
    chain = task.catch(() => {});
    return task;
  }

  async function run(id, wanted) {
    const current = getCurrent();
    // previous persisted theme; if nothing loaded, use cached or system
    const previous = current ? normalizeTheme(current.theme) : normalizeTheme(null);

    // Optimistic UI – draw and cache immediately
    try {
      draw(wanted);
    } catch {}
    try {
      cacheSet(wanted);
    } catch {}

    let saved;
    try {
      saved = await call("update_settings", { patch: { theme: wanted } });
    } catch (e) {
      // Persist failed – revert only if this is still the latest request.
      // If a newer request has already enqueued and drawn its own wanted,
      // its optimistic draw is already on screen; reverting to `previous`
      // would incorrectly overwrite the newer choice.
      if (id === seq) {
        try {
          draw(previous);
        } catch {}
        try {
          cacheSet(previous);
        } catch {}
      }
      notify(errorText(e));
      return false;
    }

    // Persist succeeded – update live settings from the backend's answer
    // (the backend clones and returns the new view, so saved.theme is canonical)
    try {
      setCurrent(saved);
    } catch {}
    const persisted = normalizeTheme(saved && saved.theme != null ? saved.theme : wanted);
    // Ensure cache reflects the canonical persisted value
    try {
      cacheSet(persisted);
    } catch {}
    if (persisted !== wanted && id === seq) {
      try {
        draw(persisted);
      } catch {}
    }

    // Native window theme – cosmetic only, never reverts the saved setting
    try {
      await applyNative(persisted);
    } catch {
      // Ignore – not a save failure. The saved preference stays.
    }
    return true;
  }

  async function setTheme(pref) {
    return enqueue(pref);
  }

  // For tests: expose queue state
  function _seq() {
    return seq;
  }

  return { setTheme, _seq };
}
