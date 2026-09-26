import test from "node:test";
import assert from "node:assert/strict";

import {
  CATEGORIES,
  safeColor,
  iconLetters,
  tileModel,
  groupTiles,
  validateSiteUrl,
  isPrivateHost,
  validateDraft,
} from "../src/logic/tiles.js";

// ---------------------------------------------------------------- safeColor

test("safeColor accepts 3- and 6-digit hex, lowercased", () => {
  assert.equal(safeColor("#ABC"), "#abc");
  assert.equal(safeColor("ff8800"), "#ff8800");
  assert.equal(safeColor("  #123456  "), "#123456");
});

test("safeColor rejects anything that is not a hex colour", () => {
  // These end up in `style.background`, so a pass-through would be an
  // injection sink.
  for (const bad of [
    "",
    "#12345",
    "#1234567",
    "red; background:url(javascript:alert(1))",
    "url(x)",
    "12px solid red",
    null,
    undefined,
    42,
    {},
  ]) {
    assert.equal(safeColor(bad), null, `should reject ${JSON.stringify(bad)}`);
  }
});

// ------------------------------------------------------------- iconLetters

test("iconLetters takes two alphanumerics and uppercases", () => {
  assert.equal(iconLetters("OpenAnime"), "OP");
  assert.equal(iconLetters("My Site"), "MS");
  assert.equal(iconLetters("Kitsu", "KI"), "KI");
});

test("iconLetters strips markup and falls back", () => {
  assert.equal(iconLetters("<img src=x>"), "IM");
  assert.equal(iconLetters(""), "?");
  assert.equal(iconLetters("!!!"), "?");
  assert.equal(iconLetters("日本"), "日本");
});

test("iconLetters prefers an explicit letter", () => {
  assert.equal(iconLetters("OpenAnime", "oa"), "OA");
  // An explicit value that is all punctuation falls back to the name.
  assert.equal(iconLetters("OpenAnime", "!!"), "OP");
});

// --------------------------------------------------------------- tileModel

test("tileModel normalises a site into safe strings", () => {
  const tile = tileModel({
    id: "builtin-openanime",
    name: "OpenAnime",
    url: "https://openani.me/",
    host: "openani.me",
    category: "watching",
    icon: { kind: "letter", text: "OA", color: "#e11d48" },
    builtin: true,
  });
  assert.deepEqual(tile, {
    id: "builtin-openanime",
    name: "OpenAnime",
    url: "https://openani.me/",
    host: "openani.me",
    category: "watching",
    letters: "OA",
    color: "#e11d48",
    builtin: true,
    native: false,
  });
});

test("tileModel substitutes a fallback colour for a hostile one", () => {
  const tile = tileModel({
    id: "x",
    name: "X",
    url: "https://x.example/",
    category: "watching",
    icon: { kind: "letter", text: "X", color: "red;background:url(javascript:1)" },
  });
  assert.equal(tile.color, "#0ea5e9");
});

test("tileModel copes with a missing icon and name", () => {
  const tile = tileModel({ id: "x", url: "https://x.example/" });
  assert.equal(tile.name, "(isimsiz)");
  // Initials come from the placeholder name, which is better than "?".
  assert.equal(tile.letters, "IS");
  assert.equal(tile.category, "watching");
  assert.equal(tile.builtin, false);
});

test("tileModel maps an unknown category onto watching", () => {
  assert.equal(tileModel({ id: "a", name: "A", category: "nonsense" }).category, "watching");
});

// -------------------------------------------------------------- groupTiles

test("groupTiles always emits both categories in order", () => {
  const groups = groupTiles([]);
  assert.deepEqual(
    groups.map((g) => g.id),
    CATEGORIES.map((c) => c.id),
  );
  assert.deepEqual(groups.map((g) => g.tiles), [[], []]);
});

test("groupTiles buckets by category and preserves order", () => {
  const groups = groupTiles([
    { id: "1", name: "B", category: "watching", icon: { kind: "letter" } },
    { id: "2", name: "A", category: "tracking", icon: { kind: "letter" } },
    { id: "3", name: "C", category: "watching", icon: { kind: "letter" } },
  ]);
  const tracking = groups.find((g) => g.id === "tracking");
  const watching = groups.find((g) => g.id === "watching");
  assert.deepEqual(tracking.tiles.map((t) => t.id), ["2"]);
  assert.deepEqual(watching.tiles.map((t) => t.id), ["1", "3"]);
});

// ---------------------------------------------------------- validateSiteUrl

test("validateSiteUrl accepts https and normalises it", () => {
  const r = validateSiteUrl("https://openani.me");
  assert.equal(r.ok, true);
  assert.equal(r.normalized, "https://openani.me/");
});

test("validateSiteUrl keeps path and query", () => {
  const r = validateSiteUrl("https://animecix.com/series/x?tab=1");
  assert.equal(r.ok, true);
  assert.equal(r.normalized, "https://animecix.com/series/x?tab=1");
});

test("validateSiteUrl refuses http rather than upgrading it", () => {
  const r = validateSiteUrl("http://openani.me");
  assert.equal(r.ok, false);
  assert.equal(r.code, "insecure_scheme");
});

test("validateSiteUrl refuses non-http schemes", () => {
  for (const u of ["javascript:alert(1)", "data:text/html,x", "file:///etc/passwd", "ftp://x/y"]) {
    assert.equal(validateSiteUrl(u).ok, false, u);
  }
});

test("validateSiteUrl refuses empty and oversized input", () => {
  assert.equal(validateSiteUrl("").ok, false);
  assert.equal(validateSiteUrl("   ").ok, false);
  assert.equal(validateSiteUrl(null).ok, false);
  assert.equal(validateSiteUrl(`https://x.example/${"a".repeat(2100)}`).ok, false);
});

test("validateSiteUrl refuses unparseable input", () => {
  assert.equal(validateSiteUrl("not a url").ok, false);
  assert.equal(validateSiteUrl("https://").ok, false);
});

test("validateSiteUrl refuses embedded credentials", () => {
  const r = validateSiteUrl("https://user:pass@example.com/");
  assert.equal(r.ok, false);
  assert.equal(r.code, "invalid_url");
});

// ------------------------------------------------------------- isPrivateHost

test("isPrivateHost catches loopback and private ranges", () => {
  for (const h of [
    "localhost",
    "sub.localhost",
    "tauri.localhost",
    "127.0.0.1",
    "10.1.2.3",
    "192.168.0.1",
    "172.16.0.1",
    "172.31.255.255",
    "169.254.169.254",
    "router.lan",
    "nas.home",
    "x.internal",
    "printer.local",
  ]) {
    assert.equal(isPrivateHost(h), true, `${h} must be private`);
  }
});

test("isPrivateHost catches CGNAT and multicast", () => {
  assert.equal(isPrivateHost("100.64.0.1"), true);
  assert.equal(isPrivateHost("100.127.0.1"), true);
  assert.equal(isPrivateHost("224.0.0.1"), true);
});

test("isPrivateHost accepts public hosts and the CGNAT boundary", () => {
  for (const h of ["openani.me", "animecix.com", "anilist.co", "1.1.1.1", "100.128.0.1"]) {
    assert.equal(isPrivateHost(h), false, `${h} must be public`);
  }
});

test("isPrivateHost handles IPv6 literals", () => {
  assert.equal(isPrivateHost("[::1]"), true);
  assert.equal(isPrivateHost("[fe80::1]"), true);
  assert.equal(isPrivateHost("[fd00::1]"), true);
  assert.equal(isPrivateHost("[2606:4700:4700::1111]"), false);
});

test("isPrivateHost does not flag lookalikes", () => {
  assert.equal(isPrivateHost("notlocalhost.example"), false);
  assert.equal(isPrivateHost("172.32.0.1"), false, "172.32 is outside 172.16/12");
});

test("validateSiteUrl refuses private hosts end to end", () => {
  for (const u of ["https://localhost/x", "https://127.0.0.1/", "https://192.168.1.1/"]) {
    assert.equal(validateSiteUrl(u).ok, false, u);
  }
});

// ------------------------------------------------------------ validateDraft

test("validateDraft requires a name", () => {
  const r = validateDraft({ name: "  ", url: "https://x.example/" });
  assert.equal(r.ok, false);
  assert.equal(r.field, "name");
});

test("validateDraft caps the name length", () => {
  const r = validateDraft({ name: "a".repeat(41), url: "https://x.example/" });
  assert.equal(r.ok, false);
  assert.equal(r.field, "name");
});

test("validateDraft reports URL problems against the url field", () => {
  const r = validateDraft({ name: "X", url: "http://x.example/" });
  assert.equal(r.ok, false);
  assert.equal(r.field, "url");
  assert.equal(r.code, undefined); // the code lives on the inner check
  assert.match(r.message, /HTTPS/);
});

test("validateDraft rejects a bad colour and letter", () => {
  assert.equal(
    validateDraft({ name: "X", url: "https://x.example/", color: "nope" }).field,
    "color",
  );
  assert.equal(
    validateDraft({ name: "X", url: "https://x.example/", letter: "!!" }).field,
    "letter",
  );
});

test("validateDraft produces a clean draft", () => {
  const r = validateDraft({
    name: "  My  Site ",
    url: "https://sub.example.org/watch",
    letter: "ms",
    color: "#ABC",
    category: "tracking",
  });
  assert.equal(r.ok, true);
  assert.deepEqual(r.draft, {
    name: "My  Site",
    url: "https://sub.example.org/watch",
    category: "tracking",
    letter: "ms",
    color: "#abc",
  });
});

test("validateDraft defaults the category to watching", () => {
  const r = validateDraft({ name: "X", url: "https://x.example/" });
  assert.equal(r.draft.category, "watching");
  assert.equal(r.draft.letter, null);
  assert.equal(r.draft.color, null);
});
