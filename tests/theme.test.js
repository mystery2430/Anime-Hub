/**
 * Theme preference logic (system/dark/light).
 */

import test from "node:test";
import assert from "node:assert/strict";

import {
  THEMES,
  THEME_CACHE_KEY,
  normalizeTheme,
  attributeFor,
  resolveTheme,
  themeLabel,
  applyThemeToDom,
} from "../src/logic/theme.js";

test("normalizeTheme rejects unknown values and falls back to system", () => {
  assert.equal(normalizeTheme("dark"), "dark");
  assert.equal(normalizeTheme("light"), "light");
  assert.equal(normalizeTheme("system"), "system");
  for (const bad of [undefined, null, "", "DARK", "auto", 42, "dark "]) {
    assert.equal(normalizeTheme(bad), "system", `${JSON.stringify(bad)} → system`);
  }
});

test("system is represented by removing data-theme so the media query drives", () => {
  assert.equal(attributeFor("system"), null);
  assert.equal(attributeFor("dark"), "dark");
  assert.equal(attributeFor("light"), "light");
  assert.equal(attributeFor("junk"), null);
  // Removing the attribute (not a "system" value) is what lets a live OS
  // change reach the CSS without any listener.
  assert.ok(!THEMES.includes(""));
});

test("resolveTheme delegates system to the media result", () => {
  assert.equal(resolveTheme("system", true), "dark");
  assert.equal(resolveTheme("system", false), "light");
  assert.equal(resolveTheme("dark", false), "dark", "explicit dark wins over light OS");
  assert.equal(resolveTheme("light", true), "light", "explicit light wins over dark OS");
});

test("themeLabel covers the three preferences in Turkish", () => {
  assert.equal(themeLabel("system"), "Sistem");
  assert.equal(themeLabel("dark"), "Koyu");
  assert.equal(themeLabel("light"), "Açık");
  assert.equal(themeLabel("?)"), "Sistem");
});

test("applyThemeToDom toggles the attribute on the document element", () => {
  const attrs = new Map();
  const fakeDoc = {
    documentElement: {
      setAttribute: (k, v) => attrs.set(k, v),
      removeAttribute: (k) => attrs.delete(k),
    },
  };
  assert.equal(applyThemeToDom("dark", fakeDoc), "dark");
  assert.equal(attrs.get("data-theme"), "dark");
  assert.equal(applyThemeToDom("light", fakeDoc), "light");
  assert.equal(attrs.get("data-theme"), "light");
  assert.equal(applyThemeToDom("system", fakeDoc), null);
  assert.ok(!attrs.has("data-theme"), "system clears the attribute");
});

test("cache key is stable for the boot-time mirror", () => {
  assert.equal(typeof THEME_CACHE_KEY, "string");
  assert.ok(THEME_CACHE_KEY.length > 0);
});
