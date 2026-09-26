import test from "node:test";
import assert from "node:assert/strict";

import { toApiError, stub, ApiError } from "../src/logic/bridge.js";

// ------------------------------------------------------------- toApiError

test("toApiError maps a structured AppError payload", () => {
  const err = toApiError({ code: "insecure_scheme", message: "Yalnızca HTTPS adresler açılabilir." });
  assert.ok(err instanceof ApiError);
  assert.equal(err.code, "insecure_scheme");
  assert.equal(err.message, "Yalnızca HTTPS adresler açılabilir.");
});

test("toApiError parses a JSON string payload", () => {
  const err = toApiError(JSON.stringify({ code: "blocked", message: "engellendi" }));
  assert.equal(err.code, "blocked");
  assert.equal(err.message, "engellendi");
});

test("toApiError falls back for unstructured errors", () => {
  assert.equal(toApiError("boom").code, "other");
  assert.equal(toApiError("boom").message, "boom");
  assert.equal(toApiError(new Error("nope")).code, "other");
  assert.equal(toApiError(undefined).code, "other");
  // A non-JSON string must not throw while being parsed.
  assert.equal(toApiError("{not json").code, "other");
});

test("toApiError never surfaces a useless message", () => {
  // `{ code: "x" }` has no message; `String(e)` would render as
  // "[object Object]" straight into the notice bar, so a real sentence is
  // substituted instead.
  const err = toApiError({ code: "x" });
  assert.equal(err.code, "other");
  assert.ok(err.message.length > 5);
  assert.ok(!err.message.includes("[object"), err.message);
});

// ------------------------------------------------------------------- stub

test("stub serves the two shipped sites", async () => {
  const sites = await stub("list_sites");
  const names = sites.map((s) => s.name);
  assert.deepEqual(names, ["OpenAnime", "AnimeCix"]);
  assert.ok(sites.every((s) => s.url.startsWith("https://")));
});

test("stub add_site normalises the icon letters", async () => {
  const added = await stub("add_site", {
    draft: { name: "Kitsu", url: "https://kitsu.app/", category: "tracking" },
  });
  assert.equal(added.icon.text, "KI");
  assert.equal(added.icon.color, "#0ea5e9");
  assert.equal(added.builtin, false);

  const after = await stub("list_sites");
  assert.ok(after.some((s) => s.name === "Kitsu"));
});

test("stub reports unsupported commands as a typed error", async () => {
  await assert.rejects(() => stub("anilist_library"), (e) => {
    assert.ok(e instanceof ApiError);
    assert.equal(e.code, "other");
    return true;
  });
});

test("stub app_info never claims encryption it does not have", async () => {
  const info = await stub("app_info");
  assert.equal(info.keyBackendOsBacked, false);
  assert.match(info.keyBackend, /şifreleme yok/);
});
