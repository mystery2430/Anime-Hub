// SPDX-License-Identifier: MIT
/**
 * Behaviour tests for `src/logic/back_close.js` — the launcher-side close
 * session behind the Android back handshake (`__animehubBackToLauncher` /
 * `__animehubBackStatus`).
 *
 * Defect A.5 regression cover: the label must survive a pending or failed
 * close (no `finally` clearing), a failed close must surface its reason,
 * a second request must never start a concurrent `close_site`, and
 * `animehub://site-closed` is accepted only for the exact opening it closes.
 *
 * The last point is subtle: site A closed and reopened still has site id
 * `id-a`, so a delayed event from the FIRST opening must be rejected by its
 * distinct native `sessionId`.
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

/** A well-formed `SiteClosedEvent` for the given opening. */
const closeEvent = (siteId, sessionId) => ({ siteId, sessionId });

test("requestClose without a session is not accepted (native fallback)", () => {
  const h = harness();
  assert.deepEqual(h.session.requestClose(), { accepted: false });
  assert.equal(h.session.statusString(), "idle");
  assert.equal(h.calls.length, 0);
});

test("native close call carries the exact opening identity", () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.requestClose();
  assert.deepEqual(h.calls[0], {
    cmd: "close_site",
    args: { label: "site-a", sessionId: "open-a-1" },
  });
});

test("opened() registers the label, site id and opening identity", () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  assert.equal(h.session.hasSession(), true);
  assert.equal(h.session.currentLabel(), "site-a");
  assert.equal(h.session.currentSessionId(), "open-a-1");
  assert.equal(h.session.statusString(), "idle");
});

test("first requestClose starts exactly one close and reports pending", async () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  const first = h.session.requestClose();
  assert.deepEqual(first, { accepted: true });
  assert.equal(h.session.statusString(), "pending");
  assert.equal(h.calls.length, 1);
  assert.deepEqual(h.calls[0], { cmd: "close_site", args: { label: "site-a", sessionId: "open-a-1" } });

  // A second back press while pending must NOT start a second close.
  const second = h.session.requestClose();
  assert.deepEqual(second, { accepted: true });
  assert.equal(h.calls.length, 1);
  assert.equal(h.session.statusString(), "pending");
});

test("native back operation id is forwarded through the launcher close bridge", () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  assert.deepEqual(h.session.requestClose("k-42"), { accepted: true });
  assert.deepEqual(h.calls[0], {
    cmd: "close_site",
    args: { label: "site-a", sessionId: "open-a-1", operationId: "k-42" },
  });
});

test("the matching close identity closes the active session normally", async () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.requestClose();
  assert.equal(h.session.currentLabel(), "site-a", "label must survive the in-flight close");
  h.resolveClose();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(h.session.statusString(), "closed");
  // Kotlin polls this while the launcher is still the visible page.
  assert.equal(h.session.statusString(), "closed");
  // The event performs the actual clear.
  assert.equal(h.session.closedByEvent(closeEvent("id-a", "open-a-1")), true);
  assert.equal(h.session.currentLabel(), null);
  assert.equal(h.session.currentSessionId(), null);
  assert.equal(h.session.statusString(), "idle");
});

test("failed close surfaces the reason, keeps the label, and is retryable", async () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
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

test("a stale close identity cannot clear someone else's session", () => {
  const h = harness();
  h.session.opened("site-b", "id-b", "open-b-1");
  assert.equal(
    h.session.closedByEvent(closeEvent("id-a", "open-a-1")),
    false,
    "another opening's event must be ignored",
  );
  assert.equal(h.session.currentLabel(), "site-b");
  assert.equal(h.session.currentSessionId(), "open-b-1");
  assert.equal(h.session.closedByEvent(closeEvent("id-b", "open-b-1")), true);
  assert.equal(h.session.currentLabel(), null);
});

test("missing or malformed identities leave the active session unchanged", () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  const malformed = [
    null,
    undefined,
    "",
    "open-a-1",
    "id-a",
    42,
    {},
    { siteId: "id-a" },
    { sessionId: "open-a-1" },
    { siteId: "", sessionId: "open-a-1" },
    { siteId: "id-a", sessionId: "" },
    { siteId: "id-a", sessionId: null },
    { siteId: "id-a", sessionId: 42 },
    { siteId: 42, sessionId: "open-a-1" },
    ["id-a", "open-a-1"],
  ];
  for (const payload of malformed) {
    assert.equal(
      h.session.closedByEvent(payload),
      false,
      `payload ${JSON.stringify(payload) ?? "undefined"} must not clear an owned session`,
    );
    assert.equal(h.session.currentLabel(), "site-a");
    assert.equal(h.session.currentSessionId(), "open-a-1");
    assert.equal(h.session.hasSession(), true);
  }
  // A well-formed event naming the wrong opening is equally stale.
  assert.equal(h.session.closedByEvent(closeEvent("id-a", "open-a-0")), false);
  assert.equal(h.session.hasSession(), true);
  // The exact owner identity still clears.
  assert.equal(h.session.closedByEvent(closeEvent("id-a", "open-a-1")), true);
  assert.equal(h.session.currentLabel(), null);
  assert.equal(h.session.statusString(), "idle");
});

test("a session without an opening identity never accepts a close event", () => {
  // Fail closed: a launcher that has no native identity to compare against
  // must not treat an unverifiable event as proof of ownership.
  const h = harness();
  h.session.opened("site-a", "id-a");
  assert.equal(h.session.closedByEvent(closeEvent("id-a", "anything")), false);
  assert.equal(h.session.hasSession(), true);
  assert.equal(h.session.currentLabel(), "site-a");

  const h2 = harness();
  h2.session.opened("site-c");
  assert.equal(h2.session.closedByEvent(closeEvent("site-c", "anything")), false);
  assert.equal(h2.session.hasSession(), true);
});

test("a delayed event from the previous session cannot clear the new one", () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.requestClose();
  h.resolveClose();
  // While the `open-a-1` event is still in flight, the user opens site-b.
  h.session.opened("site-b", "id-b", "open-b-1");
  assert.equal(
    h.session.closedByEvent(closeEvent("id-a", "open-a-1")),
    false,
    "stale opening identity must be ignored",
  );
  assert.equal(h.session.currentLabel(), "site-b");
  assert.equal(h.session.currentSessionId(), "open-b-1");
  assert.equal(h.session.statusString(), "idle");
  // An identity-less payload from the stale producer is ignored just the same.
  assert.equal(h.session.closedByEvent(null), false);
  assert.equal(h.session.currentLabel(), "site-b");
  // The new owner's own event clears.
  assert.equal(h.session.closedByEvent(closeEvent("id-b", "open-b-1")), true);
  assert.equal(h.session.currentLabel(), null);
});

test("closing and reopening the SAME site survives its old close event", async () => {
  // The race this exists for: site id is `id-a` both times.
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.requestClose();
  h.resolveClose();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(h.session.statusString(), "closed");

  // Reopen the same site before the delayed event from opening 1 arrives.
  // This mints a NEW native identity even though site id and label repeat.
  h.session.opened("site-a", "id-a", "open-a-2");
  assert.equal(h.session.currentSessionId(), "open-a-2");

  assert.equal(
    h.session.closedByEvent(closeEvent("id-a", "open-a-1")),
    false,
    "the previous opening's delayed event must not close the new session",
  );
  assert.equal(h.session.hasSession(), true);
  assert.equal(h.session.currentLabel(), "site-a");
  assert.equal(h.session.currentSessionId(), "open-a-2");
  assert.equal(h.session.statusString(), "idle");

  // The reopened session closes normally on its own event.
  assert.equal(h.session.closedByEvent(closeEvent("id-a", "open-a-2")), true);
  assert.equal(h.session.hasSession(), false);
});

test("a delayed event from opening 1 cannot close opening 2 even if opening 1 is still pending", () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.requestClose();
  // Reopen before the close resolves; the old event is still in flight.
  h.session.opened("site-a", "id-a", "open-a-2");
  assert.equal(h.session.closedByEvent(closeEvent("id-a", "open-a-1")), false);
  assert.equal(h.session.hasSession(), true);
  assert.equal(h.session.currentLabel(), "site-a");
  assert.equal(h.session.currentSessionId(), "open-a-2");
});

test("a late close result for opening 1 does not settle the reopened same site", async () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.requestClose();
  // Same label and site id, new opening; the first close result arrives after.
  h.session.opened("site-a", "id-a", "open-a-2");
  h.resolveClose();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(h.session.currentLabel(), "site-a");
  assert.equal(h.session.currentSessionId(), "open-a-2");
  assert.equal(h.session.statusString(), "idle", "the new opening starts clean");

  h.session.requestClose();
  assert.equal(h.session.statusString(), "pending");
  assert.equal(h.calls.length, 2);
});

test("opened() resets leftover failure state for the next opening", () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.requestClose();
  h.rejectClose("eski hata");
  h.session.opened("site-a", "id-a", "open-a-2");
  assert.equal(h.session.statusString(), "idle");
  assert.equal(h.session.lastError(), "");
  assert.equal(h.session.currentLabel(), "site-a");
  assert.equal(h.session.currentSessionId(), "open-a-2");
});

test("requestClose after a settled close does not re-invoke (event pending)", async () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.requestClose();
  h.resolveClose();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(h.session.statusString(), "closed");
  assert.deepEqual(h.session.requestClose(), { accepted: true });
  assert.equal(h.calls.length, 1, "closed state must not start a second flow");
});

test("reset() clears unconditionally", () => {
  const h = harness();
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.reset();
  assert.equal(h.session.hasSession(), false);
  assert.equal(h.session.currentSessionId(), null);
  assert.equal(h.session.statusString(), "idle");
});

test("custom errorText formats the rejection reason", async () => {
  const h = harness({
    errorText: (e) => `formatted:${e}`,
    notify: undefined,
  });
  h.session.opened("site-a", "id-a", "open-a-1");
  h.session.requestClose();
  h.rejectClose("raw");
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(h.session.statusString(), "failed:formatted:Error: raw");
  assert.deepEqual(h.notices, [], "no notify callback configured");
});
