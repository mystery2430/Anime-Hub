/**
 * AniList settings patch (docs here, real behaviour in settings_patch.js).
 *
 * F1: the save button used to re-send all three AniList sub-fields on every
 * save — a field that re-loads empty would therefore send blanks for values
 * the user never touched.
 */

import test from "node:test";
import assert from "node:assert/strict";

import { buildAnilistPatch } from "../src/logic/settings_patch.js";

test("untouched fields produce no anilist key at all", () => {
  const patch = buildAnilistPatch(
    [
      ["clientId", "abc"],
      ["clientSecret", "s"],
      ["redirectUri", "r"],
    ],
    new Set(),
  );
  assert.deepEqual(patch, {});
});

test("only touched keys are sent, and the secret is never trimmed", () => {
  const patch = buildAnilistPatch(
    [
      ["clientId", " abc "],
      ["clientSecret", "  sp  "],
      ["redirectUri", "u "],
    ],
    new Set(["clientSecret"]),
  );
  assert.deepEqual(patch, { anilist: { clientSecret: "  sp  " } });
});

test("touched identity fields are trimmed and coexist in one patch", () => {
  const patch = buildAnilistPatch(
    [
      ["clientId", " abc "],
      ["redirectUri", "https://ex.example/cb "],
    ],
    new Set(["clientId", "redirectUri"]),
  );
  assert.deepEqual(patch, {
    anilist: { clientId: "abc", redirectUri: "https://ex.example/cb" },
  });
});

test("a non-Set touched argument yields nothing (defensive)", () => {
  assert.deepEqual(
    buildAnilistPatch([["clientId", "abc"]], undefined),
    {},
  );
});
