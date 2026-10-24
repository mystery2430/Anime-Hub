// SPDX-License-Identifier: MIT
//
// Pins for the desktop connection-pinning proxy wiring.
//
// The Rust suite proves the proxy's behaviour on real sockets. What it cannot
// prove is that the WebView is actually pointed at it, before the first
// navigation, and that a failure never ends in an open site. Those are
// ordering properties of `web/windows.rs`, so they are pinned here against the
// shipped source, next to the behavioural tests in `pinned_proxy.rs`.

import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const read = (rel) => fs.readFileSync(path.join(ROOT, rel), "utf8");
const WINDOWS_RS = read("src-tauri/src/web/windows.rs");
const PROXY_RS = read("src-tauri/src/web/pinned_proxy.rs");
const COMMANDS_RS = read("src-tauri/src/commands.rs");
// Only the implementation matters for ordering; the test module restates
// pieces of it and would make every index ambiguous.
const OPEN_PATH = WINDOWS_RS.slice(
  WINDOWS_RS.indexOf("async fn open_in_main_window_inner"),
  WINDOWS_RS.indexOf("fn close_current_child_locked"),
);

test("the proxy starts before the WebView exists and its failure aborts the open", () => {
  const start = OPEN_PATH.indexOf("start_site_proxy().await?");
  const addChild = OPEN_PATH.indexOf(".add_child(");
  const builder = OPEN_PATH.indexOf("tauri::WebviewBuilder::new");
  assert.ok(start > 0, "the open path must start the proxy");
  assert.ok(builder > 0 && addChild > builder, "the WebView is built and then attached");
  assert.ok(
    start < builder,
    "the proxy must be running before the builder is constructed, so the first navigation is covered",
  );
  // `?` is the fail-closed half: a start error returns before any WebView call.
  assert.match(OPEN_PATH, /let proxy_port = start_site_proxy\(\)\.await\?;/);
});

test("the previous session's proxy is stopped before a new one starts", () => {
  const startFn = WINDOWS_RS.slice(
    WINDOWS_RS.indexOf("async fn start_site_proxy"),
    WINDOWS_RS.indexOf("fn stop_site_proxy"),
  );
  assert.match(
    startFn,
    /async fn start_site_proxy\(\) -> AppResult<u16> \{\s*stop_site_proxy\(\);/,
    "a stale port or handle must never serve the next session",
  );
  // Both desktop close paths release the proxy: the normal close and the
  // wipe-close used by clear/remove.
  const stops = WINDOWS_RS.match(/stop_site_proxy\(\);/g) || [];
  assert.ok(stops.length >= 4, `expected the start path, two closes and the attach failure, saw ${stops.length}`);
});

test("the WebView is pointed at this session's port on every desktop target", () => {
  assert.match(OPEN_PATH, /builder\.proxy_url\(site_proxy_url\(proxy_port\)\?\)/);
  assert.match(OPEN_PATH, /builder\.additional_browser_args\(&windows_browser_args\(proxy_port\)\)/);
  // Exactly one of the two applies per target; setting both would let wry
  // ignore the proxy (its `--proxy-server` lives in the branch that an
  // app-supplied argument string replaces).
  assert.match(
    OPEN_PATH,
    /#\[cfg\(windows\)\]\s*let builder = builder\.additional_browser_args/,
  );
  assert.match(
    OPEN_PATH,
    /#\[cfg\(all\(desktop, not\(windows\)\)\)\]\s*let builder = builder\.proxy_url/,
  );
});

test("the Windows argument string keeps the proxy and removes the loopback bypass", () => {
  const fn = WINDOWS_RS.slice(
    WINDOWS_RS.indexOf("fn windows_browser_args"),
    WINDOWS_RS.indexOf("fn site_proxy_url"),
  );
  assert.match(fn, /--proxy-server=http:\/\/127\.0\.0\.1:\{port\}/);
  assert.match(fn, /--proxy-bypass-list=<-loopback>/);
  // wry's defaults are replaced, not merged, so they have to be restated.
  const defaults = WINDOWS_RS.match(/const WRY_DEFAULT_BROWSER_ARGS: &str =\s*"([^"]+)"/);
  assert.ok(defaults, "the default argument string must be spelled out");
  assert.match(defaults[1], /--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection/);
  assert.match(defaults[1], /--autoplay-policy=no-user-gesture-required/);
  assert.match(fn, /String::from\(WRY_DEFAULT_BROWSER_ARGS\)/);
});

test("a WebView that ignores the proxy closes the site instead of running un-pinned", () => {
  const check = OPEN_PATH.indexOf("site_proxy_requests() == Some(0)");
  assert.ok(check > 0, "the load handler must verify the proxy was used");
  // Scope to the fail-closed branch itself: `navigation-blocked` and
  // `close_site_window` also appear in the navigation and pseudo-URL paths.
  const branch = OPEN_PATH.slice(check, check + 900);
  assert.match(branch, /"animehub:\/\/navigation-blocked"/, "the user must see why the site closed");
  assert.match(branch, /close_site_window\(&app, &label\)/, "the unprotected session must be torn down");
  assert.match(PROXY_RS, /pub fn requests\(&self\) -> usize/);
  // The counter counts parsed CONNECTs before the destination decision, so a
  // refused destination still proves the WebView used the proxy.
  assert.match(PROXY_RS, /context\.requests\.fetch_add\(1, Ordering::SeqCst\);/);
});

test("the site WebView keeps TLS end to end", () => {
  // Nothing in the proxy owns a TLS stack or certificate: it is a TCP pipe.
  assert.ok(!/rustls|native_tls|TlsConnector|ServerConfig/i.test(PROXY_RS));
  assert.match(PROXY_RS, /copy_bidirectional/);
});

test("the open command awaits the now-async window path", () => {
  assert.match(COMMANDS_RS, /\.await\s*\n\s*\.map\(\|label\| OpenedSite \{/);
  assert.match(WINDOWS_RS, /pub async fn open_site_window\(/);
});
