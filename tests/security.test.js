/**
 * Guardrails that are cheap to check automatically and expensive to discover
 * by hand: the "no innerHTML" rule, no secrets in the tree, and no plaintext
 * http:// in shipped config.
 */

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, existsSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

function walk(dir, filter, out = []) {
  for (const entry of readdirSync(dir)) {
    if (entry === "node_modules" || entry.startsWith(".")) continue;
    const full = join(dir, entry);
    const st = statSync(full);
    if (st.isDirectory()) walk(full, filter, out);
    else if (filter(full)) out.push(full);
  }
  return out;
}

const read = (p) => readFileSync(p, "utf8");

// ---------------------------------------------------------------------------
// The rendering contract: no innerHTML / outerHTML / insertAdjacentHTML.
// ---------------------------------------------------------------------------

test("no source file uses an HTML-injection sink", () => {
  const files = walk(join(ROOT, "src"), (p) => /\.(js|html)$/.test(p));
  assert.ok(files.length > 0, "expected to scan some source files");

  const sinks = /(\.innerHTML|\.outerHTML|insertAdjacentHTML|document\.write\s*\()/g;
  const offenders = [];
  for (const file of files) {
    const text = read(file);
    const hits = text.match(sinks);
    if (hits) offenders.push(`${relative(ROOT, file)}: ${hits.join(", ")}`);
  }
  assert.deepEqual(
    offenders,
    [],
    "User-controlled site names must go through textContent, never an HTML sink",
  );
});

test("the built bundle contains no HTML-injection sink either", () => {
  const dist = join(ROOT, "dist", "assets");
  if (!existsSync(dist)) return; // not built yet in this run
  const files = walk(dist, (p) => p.endsWith(".js"));
  const offenders = files.filter((f) => /(\.innerHTML|insertAdjacentHTML)/.test(read(f)));
  assert.deepEqual(offenders.map((f) => relative(ROOT, f)), []);
});

// ---------------------------------------------------------------------------
// No secrets, and HTTPS everywhere.
// ---------------------------------------------------------------------------

// Built from fragments so this file does not trip its own scan.
const SECRET_PATTERNS = [
  /client_secret\s*[:=]\s*["'][A-Za-z0-9_\-]{16,}["']/,
  new RegExp(["BEGIN", "(RSA |EC )?PRIVATE", "KEY"].join(" ")),
  /password\s*[:=]\s*["'][^"']{8,}["']/i,
  new RegExp(["aws", "secret", "access", "key"].join("_")),
  /AKIA[0-9A-Z]{16}/,
];

test("no credential-looking literal in tracked text files", () => {
  const files = walk(ROOT, (p) =>
    /\.(js|rs|json|toml|md|yml|yaml|kt|html|css|py|sh)$/i.test(p),
  );
  const offenders = [];
  for (const file of files) {
    const text = read(file);
    for (const re of SECRET_PATTERNS) {
      if (re.test(text)) offenders.push(`${relative(ROOT, file)} matches ${re}`);
    }
  }
  assert.deepEqual(offenders, []);
});

test("shipped config never points at plaintext http", () => {
  const conf = JSON.parse(read(join(ROOT, "src-tauri", "tauri.conf.json")));
  const serialised = JSON.stringify(conf);
  // devUrl is the one legitimate http:// (the Vite dev server on loopback).
  // Two legitimate exceptions, both loopback-only:
  //  - `build.devUrl` is the Vite dev server,
  //  - `http://ipc.localhost` is the origin Tauri itself requires in the CSP
  //    for IPC on Windows/Android.
  const withDevUrlRemoved = serialised.replaceAll(conf.build.devUrl ?? "\u0000", "");
  const withoutIpc = withDevUrlRemoved.replaceAll("http://ipc.localhost", "");
  assert.ok(
    !/http:\/\/(?!localhost|127\.0\.0\.1)/.test(withoutIpc),
    `tauri.conf.json references a non-loopback http:// origin: ${withoutIpc.match(/http:\/\/[^"']*/g)}`,
  );
  assert.equal(conf.app.security.assetProtocol.enable, false);
});

test("the CSP forbids remote scripts and frames", () => {
  const conf = JSON.parse(read(join(ROOT, "src-tauri", "tauri.conf.json")));
  const csp = conf.app.security.csp;
  assert.match(csp, /script-src 'self'/);
  assert.match(csp, /frame-src 'none'/);
  assert.match(csp, /object-src 'none'/);
  assert.ok(!/script-src[^;]*\*/.test(csp), "no wildcard script-src");
  assert.ok(!/unsafe-eval/.test(csp), "no eval");
});

test("capabilities are scoped to the launcher webview only", () => {
  const cap = JSON.parse(
    read(join(ROOT, "src-tauri", "capabilities", "default.json")),
  );
  assert.deepEqual(cap.webviews, ["main"]);
  // Sites are child webviews inside the main window, so a window-scoped
  // grant would silently cover them too.
  assert.ok(
    !cap.windows || cap.windows.length === 0,
    "window-scoped grants would cover the site webviews",
  );
  const ids = cap.permissions.map((p) => (typeof p === "string" ? p : p.identifier));
  assert.ok(!ids.includes("shell:allow-execute"), "no shell execution");
  // Site WebViews must not be able to open arbitrary URLs.
  const opener = cap.permissions.find(
    (p) => typeof p === "object" && p.identifier === "opener:allow-open-url",
  );
  assert.ok(opener, "opener permission should be explicitly scoped");
  assert.deepEqual(opener.allow, [{ url: "https://**" }]);
});

test("gitignore covers key material and build output", () => {
  const gi = read(join(ROOT, ".gitignore"));
  for (const needle of [
    "node_modules",
    "dist",
    ".env",
    "*.keystore",
    "*.jks",
    "gen/schemas",
    "gen/android",
  ]) {
    assert.ok(gi.includes(needle), `.gitignore must list ${needle}`);
  }
});
