import test from "node:test";
import assert from "node:assert/strict";

import { hasNoHiddenSites, hiddenSiteRows } from "../src/logic/hidden_sites.js";

const site = (over = {}) => ({
  id: "builtin-openanime",
  name: "OpenAnime",
  hidden: false,
  builtin: true,
  ...over,
});

test("hiddenSiteRows shows only hidden entries", () => {
  const rows = hiddenSiteRows([
    site({ id: "visible-1", name: "Visible" }),
    site({ id: "hidden-1", name: "Hidden one", hidden: true }),
  ]);
  assert.deepEqual(
    rows.map((row) => row.id),
    ["hidden-1"],
  );
});

test("hiddenSiteRows keeps the builtin flag and sorts by name", () => {
  const rows = hiddenSiteRows([
    site({ id: "b", name: "Zeta", hidden: true }),
    site({ id: "a", name: "Alpha", hidden: true, builtin: false }),
  ]);
  assert.deepEqual(
    rows.map((row) => row.name),
    ["Alpha", "Zeta"],
  );
  assert.equal(rows[0].builtin, false);
  assert.equal(rows[1].builtin, true);
});

test("hiddenSiteRows rejects malformed entries instead of rendering them", () => {
  const rows = hiddenSiteRows([
    null,
    undefined,
    "builtin-openanime",
    { hidden: true },
    { id: "", name: "no id", hidden: true },
    { id: "x", name: 42, hidden: true },
    { id: "y", name: "no flag" },
  ]);
  assert.deepEqual(rows, []);
});

test("hiddenSiteRows caps the name and strips control characters", () => {
  const rows = hiddenSiteRows([
    site({ id: "long", name: `A\u0000B\nC${"z".repeat(200)}`, hidden: true }),
  ]);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].name.startsWith("ABCzz"), true);
  assert.equal(rows[0].name.length, 80);
  assert.equal(/[\u0000-\u001f\u007f]/.test(rows[0].name), false);
});

test("hiddenSiteRows strips bidi overrides so a name cannot reorder the row", () => {
  // A stored name is data, not layout: U+202E would flip the visual order of
  // everything rendered after it, letting a hidden entry masquerade as another
  // site next to its "Geri göster" button.
  const rows = hiddenSiteRows([
    site({ id: "bidi", name: "Anime\u202EHidden", hidden: true }),
    site({ id: "isolate", name: "A\u2066B\u2069C", hidden: true }),
    site({ id: "c1", name: "X\u0085Y\u009fZ", hidden: true }),
    site({ id: "sep", name: "Line\u2028Break", hidden: true }),
  ]);
  const byId = Object.fromEntries(rows.map((row) => [row.id, row.name]));
  assert.equal(byId.bidi, "AnimeHidden");
  assert.equal(byId.isolate, "ABC");
  assert.equal(byId["c1"], "XYZ");
  assert.equal(byId.sep, "LineBreak");
  for (const name of Object.values(byId)) {
    assert.equal(
      /[\u061c\u200b-\u200f\u2028\u2029\u202a-\u202e\u2060-\u2064\u2066-\u206f\ufeff\u0080-\u009f]/.test(name),
      false,
      `${JSON.stringify(name)} still carries an invisible/format character`,
    );
  }
});

test("hiddenSiteRows falls back for a blank name", () => {
  const rows = hiddenSiteRows([site({ id: "blank", name: "   ", hidden: true })]);
  assert.equal(rows[0].name, "Adsız site");
});

test("hasNoHiddenSites mirrors the rendered list", () => {
  assert.equal(hasNoHiddenSites([]), true);
  assert.equal(hasNoHiddenSites([site()]), true);
  assert.equal(hasNoHiddenSites([site({ hidden: true })]), false);
  assert.equal(hasNoHiddenSites(null), true);
});
