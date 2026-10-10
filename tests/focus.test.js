import test from "node:test";
import assert from "node:assert/strict";
import { wrapFocusIndex } from "../src/logic/focus.js";

test("Tab moves forward inside the dialog and wraps from the last item", () => {
  assert.equal(wrapFocusIndex(3, 2, false), 0);
  assert.equal(wrapFocusIndex(3, 0, false), null);
  assert.equal(wrapFocusIndex(3, 1, false), null);
});

test("Shift+Tab wraps from the first item to the last", () => {
  assert.equal(wrapFocusIndex(3, 0, true), 2);
  assert.equal(wrapFocusIndex(3, 2, true), null);
});

test("focus outside the dialog is pulled back in", () => {
  assert.equal(wrapFocusIndex(3, -1, false), 0);
  assert.equal(wrapFocusIndex(3, -1, true), 2);
});

test("a dialog with nothing focusable yields no target", () => {
  assert.equal(wrapFocusIndex(0, -1, false), null);
});

test("a single focusable item always wraps to itself", () => {
  assert.equal(wrapFocusIndex(1, 0, false), 0);
  assert.equal(wrapFocusIndex(1, 0, true), 0);
});
