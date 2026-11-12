/**
 * AniList settings patch gate.
 *
 * F1: the save button used to fire a no-op save every time it was pressed
 * without any edit. The Rust patch type takes the WHOLE AniListConfig
 * object (required String fields), so the gate is binary: nothing touched →
 * no `anilist` key at all; any touch → all three fields, exactly as the
 * pre-merge backend contract expects.
 */

import test from "node:test";
import assert from "node:assert/strict";

import { buildAnilistPatch } from "../src/logic/settings_patch.js";

test("untouched fields produce no anilist key at all", () => {
  const patch = buildAnilistPatch(
    [
      ["clientId", "abc"],
      ["clientSecret", ""],
      ["redirectUri", "r"],
    ],
    new Set(),
  );
  assert.deepEqual(patch, {});
});

test("any touch sends the whole object: secret raw, identity trimmed", () => {
  const patch = buildAnilistPatch(
    [
      ["clientId", " abc "],
      ["clientSecret", "  sp  "],
      ["redirectUri", "https://ex.example/cb "],
    ],
    new Set(["clientSecret"]),
  );
  assert.deepEqual(patch, {
    anilist: {
      clientId: "abc",
      clientSecret: "  sp  ",
      redirectUri: "https://ex.example/cb",
    },
  });
});

test("a non-Set touched argument yields nothing (defensive)", () => {
  assert.deepEqual(buildAnilistPatch([["clientId", "abc"]], undefined), {});
});
