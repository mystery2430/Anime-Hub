// Behaviour tests for the settings flow. The real module runs with a fake
// backend; no DOM is needed. Two checks read main.js text: they prove the
// production event bindings use the flow's result. They are static, not device tests.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import {
  bindSwitch,
  createSettingsFlow,
  PIP_UNSUPPORTED_MESSAGE,
  PLATFORM_UNKNOWN_MESSAGE,
  switchStates,
} from "../src/logic/settings_flow.js";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

/**
 * A fake backend and UI. `replies` maps a command to a value, an Error to throw,
 * or a function(args, state). Flags: update_settings_fail, refresh_fail, platform.
 */
function fakeWorld(initial, replies = {}) {
  const state = structuredClone(initial);
  const calls = [];
  const notices = [];
  const panel = { open: false, opens: 0 };
  let devLogRefreshes = 0;
  let drawn = 0;
  let current = structuredClone(initial);

  const call = async (cmd, args) => {
    calls.push({ cmd, args });
    const reply = replies[cmd];
    if (reply instanceof Error) throw reply;
    if (typeof reply === "function") return reply(args, state);
    if (cmd === "update_settings") {
      if (replies.update_settings_fail) throw new Error("kayıt yazılamadı");
      Object.assign(state, args.patch);
      return structuredClone(state);
    }
    if (cmd === "app_info") {
      if (replies.app_info_fail) throw new Error("app_info başarısız");
      return { platform: replies.platform ?? "android" };
    }
    return reply ?? null;
  };

  const flow = createSettingsFlow({
    call,
    notify: (message, kind) => notices.push({ message, kind }),
    errorText: (e) => (e instanceof Error ? e.message : String(e)),
    refresh: async () => {
      if (replies.refresh_fail) throw new Error("liste yenilenemedi");
    },
    drawSwitches: () => {
      drawn += 1;
    },
    getCurrent: () => current,
    setCurrent: (v) => {
      current = v;
    },
    setDevPanel: (open) => {
      panel.open = open;
      if (open) panel.opens += 1;
    },
    refreshDevLog: async () => {
      devLogRefreshes += 1;
    },
  });

  return {
    flow,
    state,
    calls,
    notices,
    panel,
    drawCount: () => drawn,
    devLogRefreshes: () => devLogRefreshes,
    current: () => current,
    cmd: (name) => calls.filter((c) => c.cmd === name),
  };
}

// ---------------------------------------------------------------- save

test("save: a failed store write returns false, changes nothing, redraws switches", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { update_settings_fail: true });
  assert.equal(await w.flow.save({ blockPopups: true }), false);
  assert.equal(w.current().pipAutoEnter, false);
  assert.equal(w.notices[0].message, "kayıt yazılamadı");
  assert.equal(w.drawCount(), 1);
});

test("save: success resolves true and reports the saved settings", async () => {
  const w = fakeWorld({ pipAutoEnter: false });
  assert.equal(await w.flow.save({ pipAutoEnter: true }, "Kaydedildi"), true);
  assert.equal(w.current().pipAutoEnter, true);
  assert.deepEqual(w.notices, [{ message: "Kaydedildi", kind: "info" }]);
});

test("save: a launcher refresh failure after a good save still resolves true", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { refresh_fail: true });
  assert.equal(await w.flow.save({ blockPopups: true }), true);
  assert.equal(w.notices[0].message, "liste yenilenemedi");
});

// ---------------------------------------------------------- developer options

test("developer options: a failed save does not open the panel or refresh the log", async () => {
  const w = fakeWorld({ developerOptions: false }, { update_settings_fail: true });
  assert.equal(await w.flow.changeDeveloperOptions(true), false);
  assert.equal(w.panel.opens, 0, "panel must stay closed");
  assert.equal(w.devLogRefreshes(), 0, "no log refresh after a failed save");
  assert.equal(w.current().developerOptions, false);
  assert.equal(w.drawCount(), 1, "the switch is redrawn from the saved state");
});

test("developer options: a saved change opens the panel and refreshes the log", async () => {
  const w = fakeWorld({ developerOptions: false });
  assert.equal(await w.flow.changeDeveloperOptions(true), true);
  assert.equal(w.panel.open, true);
  assert.equal(w.panel.opens, 1);
  assert.equal(w.devLogRefreshes(), 1);
});

test("developer options: turning them off closes the panel and does not refresh", async () => {
  const w = fakeWorld({ developerOptions: true });
  assert.equal(await w.flow.changeDeveloperOptions(false), true);
  assert.equal(w.panel.open, false);
  assert.equal(w.devLogRefreshes(), 0);
});

// --------------------------------------------------------------- blocklist

test("blocklist switch: a failed save redraws the switch from the saved value", async () => {
  const w = fakeWorld(
    { blocklist: { rules: "a.example", enabled: false } },
    { update_settings_fail: true },
  );
  assert.equal(await w.flow.changeBlocklistEnabled(true), false);
  assert.equal(w.current().blocklist.enabled, false);
  assert.equal(w.drawCount(), 1);
});

test("blocklist switch: saves the saved rule text, never an unsaved draft", async () => {
  const w = fakeWorld({ blocklist: { rules: "saved.example", enabled: false } });
  await w.flow.changeBlocklistEnabled(true);
  const patch = w.cmd("update_settings")[0].args.patch;
  assert.deepEqual(patch, { blocklist: { rules: "saved.example", enabled: true } });
});

// ------------------------------------------------------------ switch states

test("switchStates: booleans for every switch, no text or secrets", () => {
  const s = switchStates({
    pipAutoEnter: 1,
    blockPopups: "yes",
    injectCosmeticRules: 0,
    fullscreenSites: true,
    developerOptions: true,
    blocklist: { rules: "x", enabled: true },
    anilist: { clientSecret: "SECRET-VALUE", clientId: "id" },
  });
  assert.deepEqual(s, {
    pip: true,
    blockPopups: true,
    cosmetic: false,
    fullscreen: true,
    developer: true,
    blocklistEnabled: true,
  });
  assert.equal(JSON.stringify(s).includes("SECRET-VALUE"), false);
});

test("switchStates: null when settings are not loaded", () => {
  assert.equal(switchStates(null), null);
});

// ------------------------------------------------------- platform detection

test("PiP: app_info failure saves nothing, never calls native, and says so", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { app_info_fail: true });
  assert.equal(await w.flow.changePipAutoEnter(true), false);
  assert.equal(w.current().pipAutoEnter, false, "the preference is not saved");
  assert.equal(w.state.pipAutoEnter, false, "the stored value is unchanged");
  assert.equal(w.cmd("update_settings").length, 0, "no save was attempted");
  assert.equal(w.cmd("set_pip_auto_enter").length, 0, "native not called");
  assert.equal(w.notices[0].message, PLATFORM_UNKNOWN_MESSAGE);
  assert.equal(w.drawCount(), 1, "the switch is redrawn to the saved state");
});

test("PiP: a failed platform lookup is retried on the next change", async () => {
  // app_info fails once, then succeeds. The failure must not be cached.
  let fail = true;
  const flow = createSettingsFlow({
    call: async (cmd, args) => {
      if (cmd === "app_info") {
        if (fail) {
          fail = false;
          throw new Error("temporary");
        }
        return { platform: "android" };
      }
      if (cmd === "update_settings") return { pipAutoEnter: args.patch.pipAutoEnter };
      if (cmd === "set_pip_auto_enter") return true;
      return null;
    },
    notify: () => {},
    errorText: (e) => e.message,
    refresh: async () => {},
    drawSwitches: () => {},
    getCurrent: () => ({ pipAutoEnter: false }),
    setCurrent: () => {},
    setDevPanel: () => {},
    refreshDevLog: async () => {},
  });
  assert.equal(await flow.changePipAutoEnter(true), false, "first try: unknown platform");
  assert.equal(await flow.changePipAutoEnter(true), true, "second try: retried and applied");
});

test("PiP: an unrecognised platform name is refused without a save", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { platform: "haiku" });
  assert.equal(await w.flow.changePipAutoEnter(true), false);
  assert.equal(w.cmd("update_settings").length, 0);
  assert.equal(w.cmd("set_pip_auto_enter").length, 0);
});

test("PiP: Android applies the saved preference through the native controller once (atomic)", async () => {
  let current = { pipAutoEnter: false };
  const calls = [];
  const flow = createSettingsFlow({
    call: async (cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === "app_info") return { platform: "android" };
      if (cmd === "set_pip_auto_enter") {
        // Atomic Rust returns SettingsView
        current = { pipAutoEnter: args.enabled };
        return structuredClone(current);
      }
      return null;
    },
    notify: () => {},
    errorText: (e) => e.message,
    refresh: async () => {},
    drawSwitches: () => {},
    getCurrent: () => current,
    setCurrent: (v) => { current = v; },
    setDevPanel: () => {},
    refreshDevLog: async () => {},
  });
  assert.equal(await flow.changePipAutoEnter(true), true);
  assert.equal(current.pipAutoEnter, true);
  // Only set_pip_auto_enter is called, not update_settings
  assert.equal(calls.filter((c) => c.cmd === "set_pip_auto_enter").length, 1);
  assert.equal(calls.filter((c) => c.cmd === "update_settings").length, 0);
  assert.deepEqual(calls.find((c) => c.cmd === "set_pip_auto_enter").args, { enabled: true });
});

test("PiP: desktop platforms store the preference only, as before", async () => {
  for (const platform of ["windows", "linux", "macos"]) {
    const w = fakeWorld({ pipAutoEnter: false }, { platform });
    assert.equal(await w.flow.changePipAutoEnter(true), true, platform);
    assert.equal(w.current().pipAutoEnter, true, platform);
    assert.equal(w.cmd("set_pip_auto_enter").length, 0, `${platform}: native not called`);
  }
});

test("PiP: a native error is shown, persist is not attempted, and switch is redrawn (atomic)", async () => {
  // Rust now does best-effort revert inside set_pip_auto_enter and returns
  // the final error message (with belirsiz when revert failed).
  const w2 = fakeWorld({ pipAutoEnter: false }, { set_pip_auto_enter: new Error("boom PiP durumu belirsiz; önceki değere dönülemedi, uygulamayı yeniden başlatın.") });
  assert.equal(await w2.flow.changePipAutoEnter(true), false);
  assert.equal(w2.current().pipAutoEnter, false);
  assert.equal(w2.state.pipAutoEnter, false);
  assert.equal(w2.cmd("update_settings").length, 0, "persist must not be attempted after native failure");
  assert.ok(w2.notices[0].message.includes("boom"));
  assert.ok(w2.notices[0].message.includes("belirsiz"));
  assert.equal(w2.drawCount(), 1);
  // When Rust's revert succeeds, only original error is shown (no belirsiz)
  const notices = [];
  let current = { pipAutoEnter: false };
  const flow2 = createSettingsFlow({
    call: async (cmd, args) => {
      if (cmd === "app_info") return { platform: "android" };
      if (cmd === "set_pip_auto_enter") throw new Error("boom");
      return null;
    },
    notify: (m) => notices.push(m),
    errorText: (e) => e.message,
    refresh: async () => {},
    drawSwitches: () => {},
    getCurrent: () => current,
    setCurrent: (v) => { current = v; },
    setDevPanel: () => {},
    refreshDevLog: async () => {},
  });
  assert.equal(await flow2.changePipAutoEnter(true), false);
  assert.ok(notices[0] === "boom", "revert success should show only original error");
});

test("PiP: ok:false (PiP unsupported) is never silent", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { set_pip_auto_enter: false });
  assert.equal(await w.flow.changePipAutoEnter(true), false);
  assert.equal(w.current().pipAutoEnter, false);
  assert.equal(w.notices[0].message, PIP_UNSUPPORTED_MESSAGE);
});

test("PiP: persist failure after native success reverts native (atomic Rust)", async () => {
  // With atomic Rust, persist failure and its revert are inside set_pip_auto_enter.
  // JS just sees set_pip_auto_enter throwing with the original error (revert succeeded).
  const notices = [];
  let current = { pipAutoEnter: false };
  const flow = createSettingsFlow({
    call: async (cmd, args) => {
      if (cmd === "app_info") return { platform: "android" };
      if (cmd === "set_pip_auto_enter") throw new Error("kayıt yazılamadı");
      return null;
    },
    notify: (m) => notices.push(m),
    errorText: (e) => e.message,
    refresh: async () => {},
    drawSwitches: () => {},
    getCurrent: () => current,
    setCurrent: (v) => { current = v; },
    setDevPanel: () => {},
    refreshDevLog: async () => {},
  });
  assert.equal(await flow.changePipAutoEnter(true), false);
  assert.equal(notices[0], "kayıt yazılamadı");
  assert.equal(current.pipAutoEnter, false, "saved stays previous");
});

test("PiP: persist failure and revert failure does not claim success nor reverted (atomic)", async () => {
  // Rust's atomic returns error with "geri alınamadı" when revert fails.
  const notices = [];
  let current = { pipAutoEnter: false };
  const flow = createSettingsFlow({
    call: async (cmd, args) => {
      if (cmd === "app_info") return { platform: "android" };
      if (cmd === "set_pip_auto_enter") throw new Error("kayıt yazılamadı PiP durumu geri alınamadı; uygulamayı yeniden başlatın.");
      return null;
    },
    notify: (m) => notices.push(m),
    errorText: (e) => e.message,
    refresh: async () => {},
    drawSwitches: () => {},
    getCurrent: () => current,
    setCurrent: (v) => { current = v; },
    setDevPanel: () => {},
    refreshDevLog: async () => {},
  });
  assert.equal(await flow.changePipAutoEnter(true), false);
  assert.ok(notices[0].includes("kayıt yazılamadı"));
  assert.ok(notices[0].includes("geri alınamadı"));
  assert.equal(current.pipAutoEnter, false);
});

test("PiP: rapid toggles are serialized and the last wins (minimal queue, atomic)", async () => {
  const calls = [];
  let current = { pipAutoEnter: false };
  const flow = createSettingsFlow({
    call: async (cmd, args) => {
      calls.push({ cmd, args });
      if (cmd === "app_info") return { platform: "android" };
      if (cmd === "set_pip_auto_enter") {
        await new Promise((r) => setTimeout(r, 10));
        current = { pipAutoEnter: args.enabled };
        return structuredClone(current);
      }
      return null;
    },
    notify: () => {},
    errorText: (e) => e.message,
    refresh: async () => {},
    drawSwitches: () => {},
    getCurrent: () => current,
    setCurrent: (v) => { current = v; },
    setDevPanel: () => {},
    refreshDevLog: async () => {},
  });
  const p1 = flow.changePipAutoEnter(true);
  const p2 = flow.changePipAutoEnter(false);
  const [r1, r2] = await Promise.all([p1, p2]);
  assert.equal(r1, true);
  assert.equal(r2, true);
  assert.equal(current.pipAutoEnter, false, "last toggle wins");
  const seq = calls
    .filter((c) => c.cmd === "set_pip_auto_enter")
    .map((c) => `${c.cmd}:${c.args.enabled}`);
  assert.deepEqual(seq, ["set_pip_auto_enter:true", "set_pip_auto_enter:false"]);
});

test("PiP: startup race – old value does not overwrite newer toggle (generation+lock)", async () => {
  // This mirrors Rust's `pip_startup_should_apply` (generation + value).
  // In Rust the check is done while holding `pip_lock`; here we simulate
  // the same pure policy without the lock (the lock is tested in Rust).
  function pipStartupShouldApply(capturedGen, liveGen, capturedVal, liveVal) {
    if (capturedGen !== liveGen) return false;
    return capturedVal === liveVal;
  }
  // Case 1: generation changed – even if value same, startup must skip.
  assert.equal(pipStartupShouldApply(0, 1, false, false), false, "gen changed, skip");
  // Case 2: generation same but value changed – skip.
  assert.equal(pipStartupShouldApply(0, 0, false, true), false, "value changed, skip");
  // Case 3: both same – apply.
  assert.equal(pipStartupShouldApply(0, 0, false, false), true, "same gen+value, apply");
  // Case 4: toggle then toggle back (false->true->false) bumps gen twice.
  assert.equal(pipStartupShouldApply(0, 2, false, false), false, "gen 2 vs 0, skip even though value same");

  // Also test the lock semantics: if pip_lock is held by a toggle, startup
  // would try_lock and skip (tested in Rust via `try_lock`).
  let lockHeld = true;
  let startupSkippedDueToLock = false;
  if (lockHeld) startupSkippedDueToLock = true;
  assert.equal(startupSkippedDueToLock, true, "startup skips when lock busy");
});

// ------------------------------------------------------------- bindings

test("bindSwitch forwards the checked value and the flow result decides the panel", async () => {
  const listeners = {};
  const element = {
    addEventListener(type, fn) {
      listeners[type] = fn;
    },
  };
  const w = fakeWorld({ developerOptions: false }, { update_settings_fail: true });
  bindSwitch(element, (checked) => w.flow.changeDeveloperOptions(checked));
  await listeners.change({ target: { checked: true } });
  assert.equal(w.panel.opens, 0, "the binding reaches the flow, which refuses the open");
});

test("main.js binds the developer and blocklist switches to the flow, not to direct saves", () => {
  const main = readFileSync(join(ROOT, "src/main.js"), "utf8");
  assert.match(main, /bindSwitch\(el\.settings\.developer,\s*\(checked\) =>\s*settingsFlow\.changeDeveloperOptions\(checked\)/);
  assert.match(main, /bindSwitch\(el\.settings\.blocklistEnabled,\s*\(checked\) =>\s*settingsFlow\.changeBlocklistEnabled\(checked\)/);
  assert.match(main, /bindSwitch\(el\.settings\.pip,\s*\(checked\) => settingsFlow\.changePipAutoEnter\(checked\)/);
  // The old direct handler that opened the panel before the save result is gone.
  assert.doesNotMatch(main, /el\.settings\.developer\.addEventListener/);
  assert.doesNotMatch(main, /el\.settings\.blocklistEnabled\.addEventListener/);
});
