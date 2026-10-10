// Settings saves and the Android "auto PiP" switch.
//
// The UI-free logic lives here so it can be tested with plain Node. main.js
// passes in the real backend call, notices and redraw functions.
//
// Rules:
// - A save resolves true only when the backend accepted the patch. The backend
//   changes its live settings only after the write succeeds, so a failed save
//   leaves nothing to undo on the native side.
// - The PiP switch saves the preference first and tells the native controller
//   only after that. If the native side throws or refuses (`ok: false`, PiP
//   unsupported), the saved value is rolled back and the user is told.
// - Desktop: the preference is stored; there is no native controller to call.

/**
 * @param {object} deps
 * @param {(cmd: string, args?: object) => Promise<any>} deps.call
 * @param {(message: string, kind?: string) => void} deps.notify
 * @param {(error: unknown) => string} deps.errorText
 * @param {() => Promise<void>} deps.refresh  launcher refresh after a save
 * @param {() => void} deps.drawSwitches  redraw switches from the saved state
 * @param {() => object | null} deps.getCurrent
 * @param {(value: object) => void} deps.setCurrent
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

  /** The platform from the backend, read once. Empty string if unknown. */
  async function currentPlatform() {
    if (platform === null) {
      try {
        const info = await call("app_info");
        platform = typeof info?.platform === "string" ? info.platform : "";
      } catch {
        return "";
      }
    }
    return platform;
  }

  /**
   * Undo the saved preference, then draw the switches from what is stored. If
   * the undo save also fails, the switch shows the stored value, not a guess.
   */
  async function rollbackPip(previous, message) {
    notify(message);
    await save({ pipAutoEnter: previous });
    drawSwitches();
  }

  /** The "auto PiP" switch. See the module comment for the rules. */
  async function changePipAutoEnter(wanted) {
    const previous = Boolean(getCurrent()?.pipAutoEnter);
    if (!(await save({ pipAutoEnter: wanted }))) return false;
    if ((await currentPlatform()) !== "android") return true; // desktop: stored only
    let applied;
    try {
      applied = await call("set_pip_auto_enter", { enabled: wanted });
    } catch (e) {
      await rollbackPip(previous, errorText(e));
      return false;
    }
    if (applied !== true) {
      await rollbackPip(
        previous,
        "Bu cihazda resim içinde resim (PiP) desteklenmiyor.",
      );
      return false;
    }
    return true;
  }

  return { save, changePipAutoEnter };
}
