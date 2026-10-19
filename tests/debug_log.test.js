// Safe projection of the mixed Rust/Kotlin diagnostics payload.
// These are real production transforms, not source-text checks.
import test from "node:test";
import assert from "node:assert/strict";
import {
  sanitizeDebugLogSnapshot,
  serializeDebugLogSnapshot,
} from "../src/logic/debug_log.js";
import { createDeveloperLogReader } from "../src/logic/dev_log_refresh.js";

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function event({
  sequence,
  source,
  timestampMs,
  sourceSequence = sequence,
  area = "open",
  phase = "profile_check",
  result = "present",
  operationId,
  ...extra
}) {
  return {
    sequence,
    source,
    timestampMs,
    sourceSequence,
    area,
    phase,
    result,
    ...(operationId ? { operationId } : {}),
    ...extra,
  };
}

test("mixed events sort by time, then source and source sequence while keeping correlation", () => {
  const snapshot = sanitizeDebugLogSnapshot({
    events: [
      event({
        sequence: 6,
        source: "kotlin",
        timestampMs: 200,
        area: "back",
        phase: "launcher_close",
        result: "started",
        operationId: "k-44",
        stepId: 2,
      }),
      event({
        sequence: 4,
        source: "kotlin",
        timestampMs: 100,
        sourceSequence: 2,
        phase: "profile_bind",
        result: "ok",
        operationId: "r-9",
      }),
      event({
        sequence: 7,
        source: "rust",
        timestampMs: 100,
        phase: "open",
        result: "started",
        operationId: "r-9",
      }),
      event({
        sequence: 3,
        source: "kotlin",
        timestampMs: 100,
        sourceSequence: 1,
        phase: "profile_check",
        result: "started",
        operationId: "r-9",
      }),
      event({ sequence: 1, source: "unknown", timestampMs: 1 }),
    ],
    capacity: 4_000,
    droppedEvents: 3,
  });

  assert.deepEqual(
    snapshot.events.map(({ timestampMs, source, sourceSequence, phase }) => [
      timestampMs,
      source,
      sourceSequence,
      phase,
    ]),
    [
      [100, "rust", 7, "open"],
      [100, "kotlin", 1, "profile_check"],
      [100, "kotlin", 2, "profile_bind"],
      [200, "kotlin", 6, "launcher_close"],
    ],
  );
  assert.deepEqual(snapshot.events.map((item) => item.sequence), [1, 2, 3, 4]);
  assert.ok(snapshot.events.every((item) => item.operationId === "r-9" || item.operationId === "k-44"));
  assert.equal(snapshot.droppedEvents, 3);
  assert.equal(snapshot.capacity, 4_000);
});

test("watchdog and retirement outcomes remain usable safe result codes", () => {
  const snapshot = sanitizeDebugLogSnapshot({
    events: [
      event({ sequence: 1, source: "kotlin", timestampMs: 1, area: "clear", phase: "watchdog_arm", result: "armed" }),
      event({ sequence: 2, source: "rust", timestampMs: 2, area: "delete", phase: "quarantine_retire", result: "refused" }),
    ],
  });

  assert.deepEqual(snapshot.events.map(({ result }) => result), ["armed", "refused"]);
});

test("unknown fields and sensitive strings are discarded or replaced by safe codes", () => {
  const secret = "https://private.example/watch?token=TOP-SECRET";
  const safe = sanitizeDebugLogSnapshot({
    events: [
      event({
        sequence: 1,
        source: "kotlin",
        timestampMs: 10,
        area: secret,
        phase: `profile ${secret}`,
        result: `Cookie: ${secret}`,
        operationId: "site-private-profile",
        watchdog: `Authorization: Bearer ${secret}`,
        errorClass: `Exception: ${secret}`,
        durationMs: Number.MAX_SAFE_INTEGER,
        count: 9_000_000_000,
        url: secret,
        cookie: "Set-Cookie: secret=1",
        page: "page body",
      }),
    ],
    capacity: 99_999,
  });

  const json = serializeDebugLogSnapshot(safe);
  for (const privateValue of ["private.example", "TOP-SECRET", "Authorization", "Set-Cookie", "page body", "site-private-profile"]) {
    assert.equal(json.includes(privateValue), false, `unsafe value leaked: ${privateValue}`);
  }
  assert.deepEqual(Object.keys(safe.events[0]).sort(), [
    "area",
    "count",
    "durationMs",
    "errorClass",
    "phase",
    "result",
    "sequence",
    "source",
    "sourceSequence",
    "timestampMs",
    "watchdog",
  ]);
  assert.equal(safe.events[0].area, "site");
  assert.equal(safe.events[0].phase, "complete");
  assert.equal(safe.events[0].result, "unknown");
  assert.equal(safe.events[0].watchdog, "unknown");
  assert.equal(safe.events[0].errorClass, "unknown");
  assert.equal(safe.events[0].count, 1_000_000_000);
  assert.equal(safe.events[0].durationMs, 86_400_000);
  assert.equal(safe.capacity, 4_000);
});

test("the projection caps exports at 4,000 events and carries forward the dropped count", () => {
  const events = Array.from({ length: 4_003 }, (_, index) =>
    event({
      sequence: index + 1,
      source: index % 2 === 0 ? "rust" : "kotlin",
      timestampMs: index + 1,
      operationId: index % 2 === 0 ? "r-1" : "k-1",
      count: index,
    }),
  );
  const result = sanitizeDebugLogSnapshot({ events, droppedEvents: 7, capacity: 4_000 });
  assert.equal(result.events.length, 4_000);
  assert.equal(result.droppedEvents, 10);
  assert.equal(result.capacity, 4_000);
  assert.equal(result.events[0].count, 3);
  assert.equal(result.events.at(-1).count, 4_002);
});

test("malformed snapshots produce an empty bounded schema", () => {
  assert.deepEqual(sanitizeDebugLogSnapshot({ events: "not-an-array" }), {
    events: [],
    droppedEvents: 0,
    capacity: 4_000,
  });
  assert.equal(serializeDebugLogSnapshot(null), '{\n  "events": [],\n  "droppedEvents": 0,\n  "capacity": 4000\n}\n');
});

test("a late diagnostics reply is discarded after the gate closes and the panel reopens", async () => {
  const oldRead = deferred();
  const currentRead = deferred();
  const reads = [oldRead, currentRead];
  const applied = [];
  const errors = [];
  let enabled = true;
  let visible = true;
  const reader = createDeveloperLogReader({
    isEnabled: () => enabled,
    isVisible: () => visible,
    read: () => reads.shift().promise,
    reset: async () => {},
    onSnapshot: (snapshot) => applied.push(snapshot),
    onError: (_error, operation) => errors.push(operation),
  });

  const oldRequest = reader.refresh();
  await Promise.resolve(); // let the allowed native read start
  enabled = false;
  visible = false;
  reader.invalidate();
  oldRead.resolve("stale snapshot");
  assert.equal(await oldRequest, false);
  assert.deepEqual(applied, []);

  enabled = true;
  visible = true;
  const currentRequest = reader.refresh();
  currentRead.resolve("current snapshot");
  assert.equal(await currentRequest, true);
  assert.deepEqual(applied, ["current snapshot"]);
  assert.deepEqual(errors, []);
});

test("overlapping diagnostics reads are latest-request-wins", async () => {
  const first = deferred();
  const second = deferred();
  const reads = [first, second];
  const applied = [];
  const reader = createDeveloperLogReader({
    isEnabled: () => true,
    isVisible: () => true,
    read: () => reads.shift().promise,
    reset: async () => {},
    onSnapshot: (snapshot) => applied.push(snapshot),
    onError: () => assert.fail("unexpected diagnostics read error"),
  });

  const older = reader.refresh();
  await Promise.resolve(); // let the first native read start
  const newer = reader.refresh();
  await Promise.resolve(); // let the second native read start
  second.resolve("newer");
  assert.equal(await newer, true);
  first.resolve("older");
  assert.equal(await older, false);
  assert.deepEqual(applied, ["newer"]);
});

test("reset publishes only the fresh snapshot after its reset call succeeds", async () => {
  const calls = [];
  const applied = [];
  const reader = createDeveloperLogReader({
    isEnabled: () => true,
    isVisible: () => true,
    reset: async () => calls.push("reset"),
    read: async () => {
      calls.push("read");
      return "fresh snapshot";
    },
    onSnapshot: (snapshot, operation) => applied.push([snapshot, operation]),
    onError: () => assert.fail("unexpected diagnostics operation error"),
  });

  assert.equal(await reader.resetAndRefresh(), true);
  assert.deepEqual(calls, ["reset", "read"]);
  assert.deepEqual(applied, [["fresh snapshot", "reset"]]);
});

test("a reset that completes after gate closure does not start a diagnostics read", async () => {
  const resetResult = deferred();
  const calls = [];
  const applied = [];
  let enabled = true;
  let visible = true;
  const reader = createDeveloperLogReader({
    isEnabled: () => enabled,
    isVisible: () => visible,
    reset: () => {
      calls.push("reset");
      return resetResult.promise;
    },
    read: async () => {
      calls.push("read");
      return "should not be read";
    },
    onSnapshot: (snapshot) => applied.push(snapshot),
    onError: () => assert.fail("unexpected diagnostics operation error"),
  });

  const request = reader.resetAndRefresh();
  await Promise.resolve();
  enabled = false;
  visible = false;
  reader.invalidate();
  resetResult.resolve();

  assert.equal(await request, false);
  assert.deepEqual(calls, ["reset"]);
  assert.deepEqual(applied, []);
});

test("a refresh started during reset reads only after reset settles", async () => {
  const resetResult = deferred();
  const readResult = deferred();
  const calls = [];
  const applied = [];
  let readStarted;
  const didReadStart = new Promise((resolve) => {
    readStarted = resolve;
  });
  const reader = createDeveloperLogReader({
    isEnabled: () => true,
    isVisible: () => true,
    reset: () => {
      calls.push("reset");
      return resetResult.promise;
    },
    read: () => {
      calls.push("read");
      readStarted();
      return readResult.promise;
    },
    onSnapshot: (snapshot, operation) => applied.push([snapshot, operation]),
    onError: () => assert.fail("unexpected diagnostics operation error"),
  });

  const resetRequest = reader.resetAndRefresh();
  await Promise.resolve();
  const refreshRequest = reader.refresh();
  assert.deepEqual(calls, ["reset"]);

  resetResult.resolve();
  await didReadStart;
  assert.deepEqual(calls, ["reset", "read"]);
  readResult.resolve("after reset");
  assert.equal(await refreshRequest, true);
  assert.equal(await resetRequest, false, "superseded reset must not publish a stale snapshot");
  assert.deepEqual(applied, [["after reset", "refresh"]]);
});
