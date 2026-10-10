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
  const w = fakeWorld({ pipAutoEnter: false }, { platform: "ios" });
  assert.equal(await w.flow.changePipAutoEnter(true), false);
  assert.equal(w.cmd("update_settings").length, 0);
  assert.equal(w.cmd("set_pip_auto_enter").length, 0);
});

test("PiP: Android applies the saved preference through the native controller once", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { set_pip_auto_enter: true });
  assert.equal(await w.flow.changePipAutoEnter(true), true);
  assert.equal(w.current().pipAutoEnter, true);
  assert.equal(w.cmd("update_settings").length, 1);
  assert.deepEqual(w.cmd("set_pip_auto_enter").map((c) => c.args), [{ enabled: true }]);
});

test("PiP: desktop platforms store the preference only, as before", async () => {
  for (const platform of ["windows", "linux", "macos"]) {
    const w = fakeWorld({ pipAutoEnter: false }, { platform });
    assert.equal(await w.flow.changePipAutoEnter(true), true, platform);
    assert.equal(w.current().pipAutoEnter, true, platform);
    assert.equal(w.cmd("set_pip_auto_enter").length, 0, `${platform}: native not called`);
  }
});

test("PiP: a native error rolls the saved value back", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { set_pip_auto_enter: new Error("boom") });
  assert.equal(await w.flow.changePipAutoEnter(true), false);
  assert.equal(w.current().pipAutoEnter, false);
  assert.equal(w.state.pipAutoEnter, false);
  assert.equal(w.notices[0].message, "boom");
});

test("PiP: ok:false (PiP unsupported) is never silent", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { set_pip_auto_enter: false });
  assert.equal(await w.flow.changePipAutoEnter(true), false);
  assert.equal(w.current().pipAutoEnter, false);
  assert.equal(w.notices[0].message, PIP_UNSUPPORTED_MESSAGE);
});

test("PiP: when the rollback save also fails, the stored value is what is shown", async () => {
  let saves = 0;
  let drawn = 0;
  const notices = [];
  const flow = createSettingsFlow({
    call: async (cmd) => {
      if (cmd === "update_settings") {
        saves += 1;
        if (saves === 1) return { pipAutoEnter: true };
        throw new Error("rollback failed");
      }
      if (cmd === "app_info") return { platform: "android" };
      if (cmd === "set_pip_auto_enter") throw new Error("boom");
      return null;
    },
    notify: (m, k) => notices.push({ message: m, kind: k }),
    errorText: (e) => e.message,
    refresh: async () => {},
    drawSwitches: () => {
      drawn += 1;
    },
    getCurrent: () => ({ pipAutoEnter: false }),
    setCurrent: () => {},
    setDevPanel: () => {},
    refreshDevLog: async () => {},
  });
  assert.equal(await flow.changePipAutoEnter(true), false);
  assert.equal(saves, 2);
  assert.ok(drawn >= 2);
  assert.equal(notices[0].message, "boom");
  assert.equal(notices.at(-1).message, "rollback failed");
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
