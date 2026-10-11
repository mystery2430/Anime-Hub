// Settings saves, switch redraws and the Android "auto PiP" switch.
//
// The UI-free logic lives here so it can be tested with plain Node. main.js
// passes in the real backend call, notices, the developer panel and the
// redraw functions, and binds DOM events through `bindSwitch`.
//
// Rules:
// - A save resolves true only when the backend accepted the patch. The backend
//   changes its live settings only after the write succeeds.
// - After a failed save, every switch is redrawn from the last saved state
//   (`switchStates(current)`), so a control never shows an unsaved value.
// - The developer panel opens and the log refreshes only after the save succeeds.
// - The blocklist switch saves the saved rule text, not the unsaved draft in
//   the textarea.
// - The PiP switch needs a known platform before it saves anything. Android
//   calls the native controller after the save. Windows, Linux and macOS store
//   the preference only. An unknown platform is refused without a save.
// - Secrets (the AniList client secret) are never part of these values.

/** The platforms this flow knows. Anything else is "unknown". */
const KNOWN_PLATFORMS = ["android", "windows", "linux", "macos"];
const DESKTOP_PLATFORMS = ["windows", "linux", "macos"];

export const PLATFORM_UNKNOWN_MESSAGE =
  "Cihaz türü belirlenemedi; PiP ayarı değiştirilmedi. Uygulamayı yeniden açıp tekrar deneyin.";
export const PIP_UNSUPPORTED_MESSAGE =
  "Bu cihazda resim içinde resim (PiP) desteklenmiyor.";

/**
 * The switch states for the saved settings, or null when nothing is loaded.
 * Only booleans leave this function; no text and no secret values.
 */
export function switchStates(settings) {
  if (!settings) return null;
  return {
    pip: Boolean(settings.pipAutoEnter),
    blockPopups: Boolean(settings.blockPopups),
    cosmetic: Boolean(settings.injectCosmeticRules),
    fullscreen: Boolean(settings.fullscreenSites),
    developer: Boolean(settings.developerOptions),
    blocklistEnabled: Boolean(settings.blocklist && settings.blocklist.enabled),
  };
}

/**
 * Attach a checkbox's change event to a handler that receives the new value.
 * The handler's result is returned to the event loop; a rejected promise is
 * the handler's own problem, so this only forwards `checked`.
 */
export function bindSwitch(element, handler) {
  element.addEventListener("change", (event) => handler(event.target.checked));
}

/**
 * @param {object} deps
 * @param {(cmd: string, args?: object) => Promise<any>} deps.call
 * @param {(message: string, kind?: string) => void} deps.notify
 * @param {(error: unknown) => string} deps.errorText
 * @param {() => Promise<void>} deps.refresh  launcher refresh after a save
 * @param {() => void} deps.drawSwitches  redraw switches from the saved state
 * @param {() => object | null} deps.getCurrent
 * @param {(value: object) => void} deps.setCurrent
 * @param {(open: boolean) => void} deps.setDevPanel
 * @param {() => Promise<void>} deps.refreshDevLog
 */
export function createSettingsFlow(deps) {
  const {
    call,
    notify,
    errorText,
    refresh,
    drawSwitches,
    getCurrent,
    setCurrent,
    setDevPanel,
    refreshDevLog,
  } = deps;
  let platform = null;

  /** Save a patch. Resolves true only when the backend accepted it. */
  async function save(patch, okMessage) {
    let saved;
    try {
      saved = await call("update_settings", { patch });
    } catch (e) {
      notify(errorText(e));
      drawSwitches();
      return false;
    }
    setCurrent(saved);
    if (okMessage) notify(okMessage, "info");
    try {
      await refresh();
    } catch (e) {
      // The save already succeeded; only the launcher refresh failed.
      notify(errorText(e));
    }
    return true;
  }

  /**
   * The platform from the backend. Cached only after a good answer, so a
   * failed `app_info` is retried next time. Empty string means unknown.
   */
  async function resolvePlatform() {
    if (platform) return platform;
    try {
      const info = await call("app_info");
      const name = info && typeof info.platform === "string" ? info.platform : "";
      if (KNOWN_PLATFORMS.includes(name)) platform = name;
      return name;
    } catch {
      return "";
    }
  }

  /** Developer options: the panel and the log refresh follow a saved change only. */
  async function changeDeveloperOptions(enabled) {
    const ok = await save({ developerOptions: enabled });
    if (!ok) return false;
    setDevPanel(enabled);
    if (enabled) await refreshDevLog();
    return true;
  }

  /** Blocklist switch: saves the saved rule text, never the unsaved draft. */
  function changeBlocklistEnabled(enabled) {
    const savedRules = (getCurrent() && getCurrent().blocklist?.rules) || "";
    return save({ blocklist: { rules: savedRules, enabled } });
  }

  // PiP: native -> persist order with compensating transaction.
  // All PiP toggles are serialized through a dedicated queue so rapid
  // clicks never interleave and the startup saved-preference native call
  // (done in Rust setup) does not race with the first toggle. Other
  // settings remain unblocked.
  let pipChain = Promise.resolve();

  async function rollbackPip(previous, message) {
    notify(message);
    await save({ pipAutoEnter: previous });
    drawSwitches();
  }

  async function doChangePip(wantedRaw) {
    const wanted = Boolean(wantedRaw);
    const name = await resolvePlatform();
    if (!KNOWN_PLATFORMS.includes(name)) {
      notify(PLATFORM_UNKNOWN_MESSAGE);
      drawSwitches();
      return false;
    }
    const previous = Boolean(getCurrent() && getCurrent().pipAutoEnter);
    if (wanted === previous) {
      drawSwitches();
      return true;
    }
    // Desktop: stored only, no native step
    if (DESKTOP_PLATFORMS.includes(name)) {
      return save({ pipAutoEnter: wanted });
    }

    // Android: native first
    let applied;
    try {
      applied = await call("set_pip_auto_enter", { enabled: wanted });
    } catch (e) {
      notify(errorText(e));
      drawSwitches();
      return false;
    }
    if (applied !== true) {
      notify(PIP_UNSUPPORTED_MESSAGE);
      drawSwitches();
      return false;
    }

    // Native succeeded – now persist. On failure, compensate native.
    let saved;
    try {
      saved = await call("update_settings", { patch: { pipAutoEnter: wanted } });
    } catch (e) {
      // Persist failed – try to revert native.
      let reverted = false;
      try {
        const rev = await call("set_pip_auto_enter", { enabled: previous });
        reverted = rev === true;
      } catch {}
      if (reverted) {
        notify(errorText(e));
      } else {
        // Compensating revert also failed – do not claim success or "reverted".
        const base = errorText(e);
        notify(base + " PiP durumu geri alınamadı; uygulamayı yeniden başlatın.");
      }
      drawSwitches();
      return false;
    }
    setCurrent(saved);
    try {
      await refresh();
    } catch (err) {
      notify(errorText(err));
    }
    return true;
  }

  /** The "auto PiP" switch. See the module comment for the rules. */
  function changePipAutoEnter(wanted) {
    const task = pipChain.then(() => doChangePip(wanted));
    // Keep chain alive even if task rejects
    pipChain = task.catch(() => {});
    return task;
  }

  return {
    save,
    changePipAutoEnter,
    changeDeveloperOptions,
    changeBlocklistEnabled,
  };
}
