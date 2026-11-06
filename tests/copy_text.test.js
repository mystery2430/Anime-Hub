import test from "node:test";
import assert from "node:assert/strict";
import { copyTextFallback } from "../src/logic/copy_text.js";

function fakeDocument({ selectError, execError, copied = true, removeError = false } = {}) {
  const children = [];
  const textarea = {
    value: "",
    style: {},
    parentNode: null,
    setAttribute() {},
    select() {
      if (selectError) throw selectError;
    },
    remove() {
      if (removeError) throw new Error("remove unavailable");
      this.parentNode?.removeChild(this);
    },
  };
  const body = {
    append(node) {
      node.parentNode = body;
      children.push(node);
    },
    removeChild(node) {
      const index = children.indexOf(node);
      if (index >= 0) children.splice(index, 1);
      node.parentNode = null;
    },
  };
  return {
    children,
    textarea,
    document: {
      body,
      createElement(name) {
        assert.equal(name, "textarea");
        return textarea;
      },
      execCommand(command) {
        assert.equal(command, "copy");
        if (execError) throw execError;
        return copied;
      },
    },
  };
}

test("clipboard fallback copies text and removes its temporary textarea", () => {
  const fixture = fakeDocument();
  assert.equal(copyTextFallback("safe diagnostics", fixture.document), true);
  assert.equal(fixture.textarea.value, "safe diagnostics");
  assert.deepEqual(fixture.children, []);
});

test("clipboard fallback removes the textarea when selection or execCommand throws", () => {
  for (const options of [
    { selectError: new Error("selection failed") },
    { execError: new Error("clipboard denied") },
  ]) {
    const fixture = fakeDocument(options);
    assert.throws(() => copyTextFallback("safe diagnostics", fixture.document));
    assert.deepEqual(fixture.children, []);
  }
});

test("clipboard fallback reports a false result and still cleans up", () => {
  const fixture = fakeDocument({ copied: false });
  assert.equal(copyTextFallback("safe diagnostics", fixture.document), false);
  assert.deepEqual(fixture.children, []);
});

test("clipboard cleanup falls back to the parent when remove() itself throws", () => {
  const fixture = fakeDocument({ removeError: true });
  assert.equal(copyTextFallback("safe diagnostics", fixture.document), true);
  assert.deepEqual(fixture.children, []);
});
