// SPDX-License-Identifier: MIT
//
// Pins for the desktop connection-pinning proxy wiring.
//
// The Rust suite proves the proxy's behaviour on real sockets. What it cannot
// prove is that the WebView is actually pointed at it, before the first
// navigation, and that a failure never ends in an open site. Those are
// ordering properties of `web/windows.rs`, so they are pinned here against the
// shipped source, next to the behavioural tests in `pinned_proxy.rs`.
//
// WHAT THESE TESTS ARE: source-string pins. Every assertion below reads the
// text of a file and checks the order of statements or the presence of a
// literal. They prove that the code says what the review agreed on; they do
// NOT prove that a WebView provider honours `proxy_url`, that macOS applies the
// data-store proxy, or that the `PageLoadEvent::Finished` check runs after a
// real un-pinned request. Only a run on each desktop WebView can show that, and
// none has happened (docs/security-residual-risks.md, round 18; round 20 explains
// why a probe cannot substitute for that run).

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
  WINDOWS_RS.indexOf("fn open_in_main_window_inner"),
  WINDOWS_RS.indexOf("fn close_current_child_locked"),
);
// The asynchronous half: binds the proxy, then takes the session lock.
const OPEN_ENTRY = WINDOWS_RS.slice(
  WINDOWS_RS.indexOf("async fn open_in_main_window("),
  WINDOWS_RS.indexOf("fn open_in_main_window_inner"),
);

test("the proxy is bound before the session lock and before the WebView exists", () => {
  const bind = OPEN_ENTRY.indexOf("start_site_proxy_handle().await?");
  const lock = OPEN_ENTRY.indexOf("lock_desktop_session()?");
  assert.ok(bind > 0, "the open path must bind the proxy");
  // Binding is the only await on the path, so it happens before the session
  // lock: a std MutexGuard alive across a suspend point would make the tauri
  // command future non-Send. Everything that touches session state, including
  // the install and the add_child call, is in the synchronous half.
  assert.ok(lock > bind, "the lock is taken after the only await, never across it");
  // `?` is the fail-closed half: a failed bind returns before the lock and
  // before any WebView call.
  assert.match(OPEN_ENTRY, /let proxy = start_site_proxy_handle\(\)\.await\?;/);
  // The handle crosses into the synchronous half, so that half cannot reach
  // the builder without one.
  assert.match(
    OPEN_ENTRY,
    /open_in_main_window_inner\(app, site, url, init_script, fullscreen, proxy\)/,
  );
});

test("the inner open installs that proxy before any WebView and cannot start its own", () => {
  assert.equal(
    OPEN_PATH.indexOf("start_site_proxy"),
    -1,
    "the synchronous half must not bind a proxy of its own: it received one",
  );
  const install = OPEN_PATH.indexOf("install_site_proxy(proxy)");
  const builder = OPEN_PATH.indexOf("tauri::WebviewBuilder::new");
  const addChild = OPEN_PATH.indexOf(".add_child(");
  assert.ok(install > 0, "the handle must become the session proxy");
  assert.ok(builder > 0 && addChild > builder, "the WebView is built and then attached");
  assert.ok(
    install < builder,
    "the proxy must be listening before the WebView is created (ordering only: " +
      "whether the provider applies it is a device question)",
  );
  // The already-open early return happens before the install: the freshly
  // bound listener is dropped there instead of leaking a port.
  const early = OPEN_PATH.indexOf("return Ok(label);");
  assert.ok(early > 0 && early < install, "an early return must not keep the new listener");
});

test("installing a new proxy stops the previous handle", () => {
  const installFn = WINDOWS_RS.slice(
    WINDOWS_RS.indexOf("fn install_site_proxy"),
    WINDOWS_RS.indexOf("fn stop_site_proxy"),
  );
  assert.match(installFn, /let port = handle\.port\(\);/);
  assert.match(installFn, /slot\.replace\(handle\)/, "the slot owns the new handle");
  assert.match(installFn, /previous\.stop\(\)/, "a stale handle must never serve the next session");
  // Both desktop close paths release the proxy: the normal close and the
  // wipe-close used by clear/remove, plus the attach failure.
  const stops = WINDOWS_RS.match(/stop_site_proxy\(\);/g) || [];
  assert.ok(stops.length >= 4, `expected two closes and the attach failure, saw ${stops.length}`);
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

test("a page load with zero proxy CONNECTs closes the site afterwards", () => {
  // The check can only run once the page has finished loading, so it closes a
  // session that already made its requests; it is not a pre-request veto.
  assert.match(
    OPEN_PATH,
    /if !matches!\(payload\.event\(\), tauri::webview::PageLoadEvent::Finished\) \{\s*return;/,
    "the zero-CONNECT check must live inside the Finished branch",
  );
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
