/**
 * Launcher hero bar + vitrin kartı mantığı.
 */

import test from "node:test";
import assert from "node:assert/strict";

import {
  formatStat,
  heroModel,
  displayHost,
  bannerStyle,
  bannerRgb,
} from "../src/logic/hero.js";

test("formatStat localizes and refuses junk", () => {
  assert.equal(formatStat(0), "0");
  assert.equal(formatStat(142), "142");
  // tr-TR groups thousands with a dot (or at least one non-digit marker).
  const big = formatStat(12345);
  assert.ok(/12\D?345/.test(big), `got ${big}`);
  for (const junk of [NaN, Infinity, -3, "12", null, undefined]) {
    assert.equal(formatStat(junk), "—", JSON.stringify(junk));
  }
});

test("heroModel tires the numbers when signed out", () => {
  const m = heroModel(null);
  assert.deepEqual(m, { episodes: "—", current: "—", completed: "—", loggedIn: false });
  assert.equal(heroModel("weird").loggedIn, false);
});

test("heroModel maps camelCase wire names", () => {
  const m = heroModel({ episodesWatched: 142, current: 6, completed: 23 });
  assert.equal(m.episodes, "142");
  assert.equal(m.current, "6");
  assert.equal(m.completed, "23");
  assert.equal(m.loggedIn, true);
});

test("displayHost strips www and survives bad input", () => {
  assert.equal(displayHost("https://www.anilist.co/x"), "anilist.co");
  assert.equal(displayHost("https://openani.me"), "openani.me");
  assert.equal(displayHost("not a url"), "");
  assert.equal(displayHost(undefined), "");
});

test("bannerStyle builds a verifiable hex-only gradient", () => {
  const s = bannerStyle("#e11d48");
  assert.ok(s.startsWith("linear-gradient(135deg,"));
  assert.match(s, /rgb\(225 29 72\)/);
  // Nothing the caller typed can leak through: only parsed hex passes.
  const evil = bannerStyle("#fff;background:url(https://evil)");
  assert.ok(!evil.includes("evil"));
  assert.ok(!evil.includes("url("));
  // 3-digit hex expands correctly.
  assert.ok(bannerStyle("#f00").includes("rgb(255 0 0)"));
  // Garbage falls back to the accent, never throws.
  assert.ok(bannerStyle("nope").includes("rgb(99 102 241)"));
});

test("bannerRgb emits a space-separated triple", () => {
  assert.equal(bannerRgb("#0ea5e9"), "14 165 233");
  assert.equal(bannerRgb(undefined), "99 102 241");
});
