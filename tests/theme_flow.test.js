// Behaviour tests for theme_flow: optimistic UI, rollback, queue, native cosmetic
import test from "node:test";
import assert from "node:assert/strict";
import { createThemeFlow } from "../src/logic/theme_flow.js";

function fakeThemeWorld(initialCurrent, replies = {}) {
  let current = initialCurrent ? { ...initialCurrent } : { theme: "system" };
  let drawn = [];
  let cached = [];
  const notices = [];
  const calls = [];
  let nativeCalls = [];

  const call = async (cmd, args) => {
    calls.push({ cmd, args });
    if (replies[cmd] instanceof Error) throw replies[cmd];
    if (typeof replies[cmd] === "function") return replies[cmd](args, current);
    if (cmd === "update_settings") {
      if (replies.update_settings_fail) throw new Error("kayıt yazılamadı");
      const patchTheme = args.patch.theme;
      current = { ...current, theme: patchTheme };
      return { ...current };
    }
    return null;
  };

  const flow = createThemeFlow({
    call,
    notify: (msg) => notices.push(msg),
    errorText: (e) => (e instanceof Error ? e.message : String(e)),
    getCurrent: () => current,
    setCurrent: (v) => {
      current = v;
    },
    draw: (pref) => drawn.push(pref),
    cacheSet: (pref) => cached.push(pref),
    applyNative: async (pref) => {
      nativeCalls.push(pref);
      if (replies.native_fail) throw new Error("native boom");
    },
  });

  return {
    flow,
    calls,
    notices,
    drawn: () => drawn,
    cached: () => cached,
    nativeCalls: () => nativeCalls,
    current: () => current,
  };
}

test("theme: optimistic draw and cache, then persist success updates current", async () => {
  const w = fakeThemeWorld({ theme: "system" });
  // need to capture previous drawn?
  assert.equal(await w.flow.setTheme("dark"), true);
  // drawn includes optimistic dark
  assert.ok(w.drawn().includes("dark"));
  assert.ok(w.cached().includes("dark"));
  assert.equal(w.current().theme, "dark");
  assert.equal(w.notices.length, 0);
  assert.ok(w.nativeCalls().includes("dark"));
});

test("theme: persist failure reverts draw and cache to previous", async () => {
  const w = fakeThemeWorld({ theme: "light" }, { update_settings_fail: true });
  assert.equal(await w.flow.setTheme("dark"), false);
  // drawn: first dark (optimistic), then revert to light
  assert.deepEqual(w.drawn(), ["dark", "light"]);
  assert.deepEqual(w.cached(), ["dark", "light"]);
  assert.equal(w.current().theme, "light", "current unchanged");
  assert.equal(w.notices[0], "kayıt yazılamadı");
  assert.equal(w.nativeCalls().length, 0, "native not called after persist failure");
});

test("theme: rapid toggles are queued, older does not overwrite newer", async () => {
  let current = { theme: "system" };
  const calls = [];
  const drawn = [];
  const flow = createThemeFlow({
    call: async (cmd, args) => {
      calls.push(args.patch.theme);
      await new Promise((r) => setTimeout(r, 10));
      current = { theme: args.patch.theme };
      return { ...current };
    },
    notify: () => {},
    errorText: (e) => e.message,
    getCurrent: () => current,
    setCurrent: (v) => {
      current = v;
    },
    draw: (p) => drawn.push(p),
    cacheSet: () => {},
    applyNative: async () => {},
  });
  const p1 = flow.setTheme("dark");
  const p2 = flow.setTheme("light");
  const [r1, r2] = await Promise.all([p1, p2]);
  assert.equal(r1, true);
  assert.equal(r2, true);
  assert.equal(current.theme, "light", "last wins");
  // drawn should have dark then light, not light then dark
  assert.deepEqual(drawn, ["dark", "light"]);
});

test("theme: native failure does not revert saved preference", async () => {
  const w = fakeThemeWorld({ theme: "system" }, { native_fail: true });
  assert.equal(await w.flow.setTheme("dark"), true);
  assert.equal(w.current().theme, "dark", "saved stays dark even though native failed");
  assert.equal(w.notices.length, 0, "native failure is cosmetic, not shown as save error");
});

test("theme: persist failure with newer enqueued does not revert newer's draw", async () => {
  // First setTheme will fail persist, but before it reverts a second setTheme has been enqueued.
  // The rollback should not overwrite the newer choice.
  let current = { theme: "system" };
  const drawn = [];
  const flow = createThemeFlow({
    call: async (cmd, args) => {
      const wanted = args.patch.theme;
      if (wanted === "dark") throw new Error("kayıt yazılamadı");
      await new Promise((r) => setTimeout(r, 5));
      current = { theme: wanted };
      return { ...current };
    },
    notify: () => {},
    errorText: (e) => e.message,
    getCurrent: () => current,
    setCurrent: (v) => {
      current = v;
    },
    draw: (p) => drawn.push(p),
    cacheSet: () => {},
    applyNative: async () => {},
  });
  const p1 = flow.setTheme("dark"); // will fail
  const p2 = flow.setTheme("light"); // enqueued after, should succeed
  const [r1, r2] = await Promise.all([p1, p2]);
  assert.equal(r1, false);
  assert.equal(r2, true);
  // drawn: dark (optimistic p1), light (optimistic p2). Revert for p1 should not happen because p2 already overwrote.
  assert.deepEqual(drawn, ["dark", "light"]);
  assert.equal(current.theme, "light");
});
