// Behaviour tests for the settings save and the Android "auto PiP" switch.
// These run the real module with a fake backend; no DOM is needed.
import test from "node:test";
import assert from "node:assert/strict";
import { createSettingsFlow } from "../src/logic/settings_flow.js";

/** A fake backend. `replies` maps a command to a value or a thrown error. */
function fakeWorld(initial, replies = {}) {
  const state = { ...initial };
  const calls = [];
  const notices = [];
  let drawn = 0;
  const call = async (cmd, args) => {
    calls.push({ cmd, args });
    const reply = replies[cmd];
    if (typeof reply === "function") return reply(args, state);
    if (reply instanceof Error) throw reply;
    if (cmd === "update_settings") {
      if (replies.update_settings_fail) throw new Error("kayıt yazılamadı");
      Object.assign(state, args.patch);
      return { ...state };
    }
    if (cmd === "app_info") return { platform: replies.platform ?? "android" };
    return reply ?? null;
  };
  let current = { ...initial };
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
  });
  return {
    flow,
    state,
    calls,
    notices,
    drawCount: () => drawn,
    current: () => current,
    nativeCalls: () => calls.filter((c) => c.cmd === "set_pip_auto_enter"),
  };
}

test("save: a failed store write returns false, changes nothing, redraws switches", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { update_settings_fail: true });
  const ok = await w.flow.save({ blockPopups: true });
  assert.equal(ok, false);
  assert.equal(w.current().pipAutoEnter, false);
  assert.equal(w.notices.length, 1);
  assert.equal(w.notices[0].message, "kayıt yazılamadı");
  assert.equal(w.drawCount(), 1, "switches are redrawn from the saved state");
});

test("save: success resolves true and reports the saved settings", async () => {
  const w = fakeWorld({ pipAutoEnter: false });
  const ok = await w.flow.save({ pipAutoEnter: true }, "Kaydedildi");
  assert.equal(ok, true);
  assert.equal(w.current().pipAutoEnter, true);
  assert.deepEqual(w.notices, [{ message: "Kaydedildi", kind: "info" }]);
});

test("save: a launcher refresh failure after a good save still resolves true", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { refresh_fail: true });
  const ok = await w.flow.save({ blockPopups: true });
  assert.equal(ok, true, "the save itself succeeded");
  assert.equal(w.notices[0].message, "liste yenilenemedi");
});

test("PiP switch: a failed save never calls the native controller", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { update_settings_fail: true });
  const ok = await w.flow.changePipAutoEnter(true);
  assert.equal(ok, false);
  assert.equal(w.nativeCalls().length, 0);
  assert.equal(w.current().pipAutoEnter, false);
});

test("PiP switch: native success is applied and returns true", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { set_pip_auto_enter: true });
  const ok = await w.flow.changePipAutoEnter(true);
  assert.equal(ok, true);
  assert.equal(w.current().pipAutoEnter, true);
  assert.deepEqual(w.nativeCalls().map((c) => c.args), [{ enabled: true }]);
  assert.equal(w.notices.length, 0);
});

test("PiP switch: a native error rolls the saved value back and tells the user", async () => {
  const w = fakeWorld(
    { pipAutoEnter: false },
    { set_pip_auto_enter: new Error("PiP ayarlanamadı") },
  );
  const ok = await w.flow.changePipAutoEnter(true);
  assert.equal(ok, false);
  assert.equal(w.current().pipAutoEnter, false, "rolled back to the previous value");
  assert.equal(w.state.pipAutoEnter, false, "the stored value is rolled back too");
  assert.equal(w.notices[0].message, "PiP ayarlanamadı");
  assert.ok(w.drawCount() >= 1, "switches redrawn from the stored state");
});

test("PiP switch: ok:false (PiP unsupported on the device) is never silent", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { set_pip_auto_enter: false });
  const ok = await w.flow.changePipAutoEnter(true);
  assert.equal(ok, false);
  assert.equal(w.current().pipAutoEnter, false);
  assert.match(w.notices[0].message, /desteklenmiyor/);
});

test("PiP switch: when the rollback save also fails, the stored value is what is shown", async () => {
  // First save succeeds, native fails, then the rollback save fails.
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
  });
  const ok = await flow.changePipAutoEnter(true);
  assert.equal(ok, false);
  assert.equal(saves, 2, "one save for the change, one for the rollback");
  assert.ok(drawn >= 2, "switches are drawn after the failed rollback too");
  assert.equal(notices[0].message, "boom");
  assert.equal(notices.at(-1).message, "rollback failed");
});

test("PiP switch: desktop stores the preference and never calls the native controller", async () => {
  const w = fakeWorld({ pipAutoEnter: false }, { platform: "windows" });
  const ok = await w.flow.changePipAutoEnter(true);
  assert.equal(ok, true);
  assert.equal(w.current().pipAutoEnter, true);
  assert.equal(w.nativeCalls().length, 0);
});

test("developer options use the same save path: a failed save reports false", async () => {
  const w = fakeWorld({ developerOptions: false }, { update_settings_fail: true });
  const ok = await w.flow.save({ developerOptions: true });
  assert.equal(ok, false);
  assert.equal(w.current().developerOptions, false);
});
