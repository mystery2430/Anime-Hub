// SPDX-License-Identifier: MIT
/**
 * Behaviour tests for `src/logic/back_close.js` — the launcher-side close
 * session behind the Android back handshake (`__animehubBackToLauncher` /
 * `__animehubBackStatus`).
 *
 * Defect A.5 regression cover: the label must survive a pending or failed
 * close (no `finally` clearing), a failed close must surface its reason,
 * a second request must never start a concurrent `close_site`, and stale
 * `animehub://site-closed` events must not orphan a newer session.
 *
 * Pure state machine + injected `call` fake — no Tauri, no DOM, no new
 * dependencies.
 */
import test from "node:test";
import assert from "node:assert/strict";

import { createBackSession } from "../src/logic/back_close.js";

/** Session wired to a controllable fake bridge. */
function harness(overrides = {}) {
  const calls = [];
  const notices = [];
  const errors = [];
  let pending = null;
  const session = createBackSession({
    call: (cmd, args) => {
      calls.push({ cmd, args });
      return new Promise((resolve, reject) => {
        pending = { resolve, reject };
      });
    },
    notify: (message) => notices.push(message),
    errorText: (e) => {
      const text = e instanceof Error ? e.message : String(e);
      errors.push(text);
      return text;
    },
    ...overrides,
  });
  return {
    session,
    calls,
    notices,
    errors,
    resolveClose: () => pending.resolve(),
    rejectClose: (message) => pending.reject(new Error(message)),
  };
}

test("requestClose without a session is not accepted (native fallback)", () => {
  const h = harness();
  assert.deepEqual(h.session.requestClose(), { accepted: false });
  assert.equal(h.session.statusString(), "idle");
  assert.equal(h.calls.length, 0);
});

test("opened() registers the label and the owning site id", () => {
  const h = harness();
  h.session.opened("site-a", "id-a");
  assert.equal(h.session.hasSession(), true);
  assert.equal(h.session.currentLabel(), "site-a");
  assert.equal(h.session.statusString(), "idle");
});

test("first requestClose starts exactly one close and reports pending", async () => {
  const h = harness();
  h.session.opened("site-a", "id-a");
  const first = h.session.requestClose();
  assert.deepEqual(first, { accepted: true });
  assert.equal(h.session.statusString(), "pending");
  assert.equal(h.calls.length, 1);
  assert.deepEqual(h.calls[0], { cmd: "close_site", args: { label: "site-a" } });

  // A second back press while pending must NOT start a second close.
  const second = h.session.requestClose();
  assert.deepEqual(second, { accepted: true });
  assert.equal(h.calls.length, 1);
  assert.equal(h.session.statusString(), "pending");
});

test("successful close settles to closed and keeps the label until the event", async () => {
  const h = harness();
  h.session.opened("site-a", "id-a");
  h.session.requestClose();
  assert.equal(h.session.currentLabel(), "site-a", "label must survive the in-flight close");
  h.resolveClose();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(h.session.statusString(), "closed");
  // Kotlin polls this while the launcher is still the visible page.
  assert.equal(h.session.statusString(), "closed");
  // The event performs the actual clear.
  assert.equal(h.session.closedByEvent("id-a"), true);
  assert.equal(h.session.currentLabel(), null);
  assert.equal(h.session.statusString(), "idle");
});

test("failed close surfaces the reason, keeps the label, and is retryable", async () => {
  const h = harness();
  h.session.opened("site-a", "id-a");
  h.session.requestClose();
  h.rejectClose("disk patladı");
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.equal(h.session.statusString(), "failed:disk patladı");
  assert.equal(h.session.lastError(), "disk patladı");
  assert.deepEqual(h.notices, ["disk patladı"], "the failure must be surfaced");
  assert.equal(h.session.currentLabel(), "site-a", "label must NOT be cleared on failure");

  // Retry: a fresh attempt is started and pending again.
  assert.deepEqual(h.session.requestClose(), { accepted: true });
  assert.equal(h.session.statusString(), "pending");
  assert.equal(h.calls.length, 2);
  h.resolveClose();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(h.session.statusString(), "closed");
});

test("a stale site-closed event cannot clear someone else's session", () => {
  const h = harness();
  h.session.opened("site-b", "id-b");
  assert.equal(h.session.closedByEvent("id-a"), false, "mismatched id must be ignored");
  assert.equal(h.session.currentLabel(), "site-b");
  assert.equal(h.session.closedByEvent("id-b"), true);
  assert.equal(h.session.currentLabel(), null);
});

test("site-closed without an owned id clears (unverifiable payload)", () => {
  const h = harness();
  h.session.opened("site-a", null);
  assert.equal(h.session.closedByEvent("anything"), true);
  assert.equal(h.session.hasSession(), false);
});

test("opened() resets leftover failure state for the next site", () => {
  const h = harness();
  h.session.opened("site-a", "id-a");
  h.session.requestClose();
  h.rejectClose("eski hata");
  h.session.opened("site-b", "id-b");
  assert.equal(h.session.statusString(), "idle");
  assert.equal(h.session.lastError(), "");
  assert.equal(h.session.currentLabel(), "site-b");
});

test("requestClose after a settled close does not re-invoke (event pending)", async () => {
  const h = harness();
  h.session.opened("site-a", "id-a");
  h.session.requestClose();
  h.resolveClose();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(h.session.statusString(), "closed");
  assert.deepEqual(h.session.requestClose(), { accepted: true });
  assert.equal(h.calls.length, 1, "closed state must not start a second flow");
});

test("reset() clears unconditionally", () => {
  const h = harness();
  h.session.opened("site-a", "id-a");
  h.session.reset();
  assert.equal(h.session.hasSession(), false);
  assert.equal(h.session.statusString(), "idle");
});

test("custom errorText formats the rejection reason", async () => {
  const h = harness({
    errorText: (e) => `formatted:${e}`,
    notify: undefined,
  });
  h.session.opened("site-a", "id-a");
  h.session.requestClose();
  h.rejectClose("raw");
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(h.session.statusString(), "failed:formatted:Error: raw");
  assert.deepEqual(h.notices, [], "no notify callback configured");
});
