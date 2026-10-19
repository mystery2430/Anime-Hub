// Safe projection of the mixed Rust/Kotlin diagnostics payload.
// These are real production transforms, not source-text checks.
import test from "node:test";
import assert from "node:assert/strict";
import {
  sanitizeDebugLogSnapshot,
  serializeDebugLogSnapshot,
} from "../src/logic/debug_log.js";

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
