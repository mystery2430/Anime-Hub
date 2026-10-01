import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { Blob, File } from "node:buffer";
import { TextDecoder, TextEncoder } from "node:util";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const KOTLIN_PATH = join(
  ROOT,
  "src-tauri/android-plugin/kotlin/dev/animehub/app/AnimeHubPlugin.kt",
);
const KOTLIN = readFileSync(KOTLIN_PATH, "utf8");

function rawKotlinString(name, source = KOTLIN) {
  const normalizedSource = source.replace(/\r+\n/g, "\n").replace(/\r/g, "\n");
  const match = normalizedSource.match(
    new RegExp(`private val ${name} = """\\n([\\s\\S]*?)\\n    """.trimIndent\\(\\)`),
  );
  if (!match) {
    const declaration = normalizedSource
      .split("\n")
      .find((line) => line.includes(`private val ${name}`));
    throw new Error(
      `could not find ${name} Kotlin raw string; declaration=${JSON.stringify(declaration ?? null)}`,
    );
  }
  const lines = match[1].split("\n");
  const indent = Math.min(
    ...lines.filter((line) => line.trim()).map((line) => line.match(/^ */)[0].length),
  );
  return lines.map((line) => line.slice(indent)).join("\n");
}

const EXPORT_JS = rawKotlinString("EXPORT_IDB_JS");
const IMPORT_JS = rawKotlinString("IMPORT_IDB_JS");

class NameList {
  constructor(readNames) {
    this.readNames = readNames;
  }

  get length() {
    return this.readNames().length;
  }

  item(index) {
    return this.readNames()[index] ?? null;
  }

  contains(name) {
    return this.readNames().includes(name);
  }
}

function keyId(key) {
  if (typeof key === "string") return `s:${key}`;
  if (typeof key === "number") return `n:${key}`;
  return `j:${JSON.stringify(key)}`;
}

function valueAtPath(value, path) {
  if (Array.isArray(path)) return path.map((part) => valueAtPath(value, part));
  return String(path).split(".").reduce((item, part) => item?.[part], value);
}

class FakeStore {
  constructor(name, options = {}) {
    this.name = name;
    this.keyPath = options.keyPath ?? null;
    this.autoIncrement = !!options.autoIncrement;
    this.indexes = new Map();
    this.records = new Map();
    this.nextKey = 1;
    this.transaction = null;
  }

  get indexNames() {
    return new NameList(() => [...this.indexes.keys()]);
  }

  createIndex(name, keyPath, options = {}) {
    if (this.indexes.has(name)) throw new Error(`duplicate index ${name}`);
    const index = {
      name,
      keyPath,
      unique: !!options.unique,
      multiEntry: !!options.multiEntry,
    };
    this.indexes.set(name, index);
    return index;
  }

  index(name) {
    const index = this.indexes.get(name);
    if (!index) throw new Error(`missing index ${name}`);
    return index;
  }

  putDirect(key, value) {
    this.records.set(keyId(key), { key, value });
  }

  getAllKeys(_query, count) {
    const keys = [...this.records.values()].map((row) => row.key);
    return requestFor(count === undefined ? keys : keys.slice(0, count));
  }

  getAll(_query, count) {
    const values = [...this.records.values()].map((row) => row.value);
    return requestFor(count === undefined ? values : values.slice(0, count));
  }

  clear() {
    this.records.clear();
    this.transaction?.scheduleComplete();
    return requestFor(undefined);
  }

  put(value, explicitKey) {
    let key = explicitKey;
    if (this.keyPath !== null) {
      key = valueAtPath(value, this.keyPath);
      if (key === undefined && this.autoIncrement) {
        key = this.nextKey++;
        if (typeof this.keyPath === "string" && !this.keyPath.includes(".")) {
          value[this.keyPath] = key;
        }
      }
    } else if (key === undefined && this.autoIncrement) {
      key = this.nextKey++;
    }
    if (key === undefined) throw new Error("missing key");
    if (typeof key === "number") this.nextKey = Math.max(this.nextKey, key + 1);
    this.records.set(keyId(key), { key, value });
    this.transaction?.scheduleComplete();
    return requestFor(key);
  }
}

class FakeDatabase {
  constructor(record) {
    this.record = record;
  }

  get name() {
    return this.record.name;
  }

  get version() {
    return this.record.version;
  }

  get objectStoreNames() {
    return new NameList(() => [...this.record.stores.keys()]);
  }

  createObjectStore(name, options = {}) {
    if (this.record.stores.has(name)) throw new Error(`duplicate store ${name}`);
    const store = new FakeStore(name, options);
    this.record.stores.set(name, store);
    return store;
  }

  deleteObjectStore(name) {
    this.record.stores.delete(name);
  }

  transaction(storeNames, _mode) {
    const names = Array.isArray(storeNames) ? storeNames : [storeNames];
    const transaction = new FakeTransaction(this.record, names);
    return transaction;
  }

  close() {}
}

class FakeTransaction {
  constructor(record, names) {
    this.record = record;
    this.names = names;
    this.oncomplete = null;
    this.onerror = null;
    this.onabort = null;
    this.scheduled = false;
  }

  objectStore(name) {
    if (!this.names.includes(name)) throw new Error(`store ${name} is outside transaction`);
    const store = this.record.stores.get(name);
    if (!store) throw new Error(`missing store ${name}`);
    store.transaction = this;
    return store;
  }

  scheduleComplete() {
    if (this.scheduled) return;
    this.scheduled = true;
    setTimeout(() => this.oncomplete?.({ target: this }), 0);
  }

  abort() {
    this.onabort?.({ target: this });
  }
}

class FakeOpenRequest {
  constructor() {
    this.result = undefined;
    this.error = null;
    this.transaction = null;
    this.onupgradeneeded = null;
    this.onsuccess = null;
    this.onerror = null;
    this.onblocked = null;
  }
}

function requestFor(value) {
  const request = { result: undefined, error: null, onsuccess: null, onerror: null };
  queueMicrotask(() => {
    request.result = value;
    request.onsuccess?.({ target: request });
  });
  return request;
}

class FakeIndexedDB {
  constructor() {
    this.records = new Map();
  }

  seed(name, version = 1) {
    const record = { name, version, stores: new Map() };
    this.records.set(name, record);
    return new FakeDatabase(record);
  }

  databases() {
    return Promise.resolve(
      [...this.records.values()].map(({ name, version }) => ({ name, version })),
    );
  }

  open(name, requestedVersion) {
    const request = new FakeOpenRequest();
    setTimeout(() => {
      let record = this.records.get(name);
      const oldVersion = record?.version ?? 0;
      const version = requestedVersion ?? record?.version ?? 1;
      if (record && version < record.version) {
        request.error = new Error("VersionError");
        request.onerror?.({ target: request });
        return;
      }
      const upgrade = !record || version > record.version;
      if (!record) {
        record = { name, version, stores: new Map() };
        this.records.set(name, record);
      }
      if (upgrade) {
        record.version = version;
        request.result = new FakeDatabase(record);
        request.transaction = {
          aborted: false,
          abort() {
            this.aborted = true;
          },
        };
        request.onupgradeneeded?.({ oldVersion, newVersion: version, target: request });
        if (request.transaction.aborted) {
          request.error = new Error("upgrade aborted");
          request.onerror?.({ target: request });
          return;
        }
      }
      request.result = new FakeDatabase(record);
      request.onsuccess?.({ target: request });
    }, 0);
    return request;
  }
}

class FakeFileReader {
  readAsDataURL(blob) {
    blob.arrayBuffer().then(
      (buffer) => {
        this.result = `data:${blob.type || "application/octet-stream"};base64,${Buffer.from(buffer).toString("base64")}`;
        this.onload?.({ target: this });
      },
      (error) => {
        this.error = error;
        this.onerror?.({ target: this });
      },
    );
  }
}

function jsContext(indexedDB) {
  const window = {
    ArrayBuffer,
    BigInt64Array,
    BigUint64Array,
    DataView,
    Float32Array,
    Float64Array,
    Int8Array,
    Int16Array,
    Int32Array,
    Uint8Array,
    Uint8ClampedArray,
    Uint16Array,
    Uint32Array,
  };
  return {
    indexedDB,
    window,
    Blob,
    File,
    FileReader: FakeFileReader,
    TextDecoder,
    TextEncoder,
    Array,
    ArrayBuffer,
    BigInt,
    Boolean,
    Date,
    DataView,
    JSON,
    Map,
    Math,
    Number,
    Object,
    Promise,
    RegExp,
    Set,
    String,
    Uint8Array,
    WeakSet,
    atob,
    btoa,
  };
}

function jsonPayload(value) {
  return Buffer.from(JSON.stringify(value), "utf8").toString("base64");
}

async function importSnapshot(indexedDB, snapshot) {
  const bridgeIndexedDB = {
    open: indexedDB.open.bind(indexedDB),
    databases() {
      throw new Error("import must not require database enumeration");
    },
  };
  const context = jsContext(bridgeIndexedDB);
  context.window.__animehub_test = "pending";
  const importFunction = vm.runInNewContext(`(${IMPORT_JS})`, context);
  return importFunction(jsonPayload(snapshot), "__animehub_test");
}

test("Kotlin raw-string extraction normalizes platform line endings", () => {
  for (const source of [
    KOTLIN.replace(/\n/g, "\r\n"),
    KOTLIN.replace(/\n/g, "\r"),
    KOTLIN.replace(/\n/g, "\r\r\n"),
  ]) {
    assert.equal(rawKotlinString("EXPORT_IDB_JS", source), EXPORT_JS);
    assert.equal(rawKotlinString("IMPORT_IDB_JS", source), IMPORT_JS);
  }
});

test("Android IndexedDB bridge JS parses and uses a bounded, explicit async bridge", () => {
  assert.doesNotThrow(() => new Function(`return (${EXPORT_JS});`));
  assert.doesNotThrow(() => new Function(`return (${IMPORT_JS});`));
  assert.match(KOTLIN, /awaitJavascriptResult\(webView, stateKey, resultKey\)/);
  assert.match(KOTLIN, /MAX_IDB_SNAPSHOT_BYTES/);
  assert.match(KOTLIN, /MAX_IDB_SEALED_CHARS/);
  assert.match(KOTLIN, /IDB_TRANSFER_CHUNK_CHARS/);
  assert.match(KOTLIN, /appendJavascriptPayload\(webView, payload, payloadKey\)/);
  assert.match(KOTLIN, /return v\.slice\(\$offset,\$end\)/);
  assert.match(KOTLIN, /Base64\.encodeToString\(json\.toByteArray\(Charsets\.UTF_8\), Base64\.NO_WRAP\)/);
});

test("v2 snapshots round-trip schema, indexes, Blob/File and structured-clone values", async () => {
  const source = new FakeIndexedDB();
  const sourceDb = source.seed("site-data", 3);
  const records = sourceDb.createObjectStore("records", {
    keyPath: "id",
    autoIncrement: true,
  });
  records.createIndex("byEmail", "email", { unique: true });
  records.createIndex("byTags", "tags", { multiEntry: true });
  const dangerousObject = JSON.parse('{"__proto__":{"polluted":true},"safe":1}');
  records.putDirect(1, {
    id: 1,
    email: "user@example.test",
    tags: ["anime", "watchlist"],
    image: new Blob([new Uint8Array([0, 1, 2, 255])], { type: "image/png" }),
    attachment: new File(["episode notes"], "notes.txt", {
      type: "text/plain",
      lastModified: 123456,
    }),
    createdAt: new Date("2025-03-04T05:06:07.000Z"),
    bytes: new Uint16Array([1, 512, 65535]),
    map: new Map([["status", "watched"]]),
    set: new Set(["dub", "sub"]),
    expression: /episode/gi,
    dangerousObject,
  });
  const logStore = sourceDb.createObjectStore("logs", { autoIncrement: true });
  logStore.putDirect(9, { event: "opened" });

  const exportFunction = vm.runInNewContext(`(${EXPORT_JS})`, jsContext(source));
  const snapshot = await exportFunction();
  assert.equal(snapshot.format, "animehub-idb");
  assert.equal(snapshot.version, 2);
  assert.equal(snapshot.databases["site-data"].version, 3);
  assert.equal(snapshot.databases["site-data"].stores.records.keyPath, "id");
  assert.equal(snapshot.databases["site-data"].stores.records.autoIncrement, true);
  assert.deepEqual(
    Array.from(snapshot.databases["site-data"].stores.records.indexes, (index) => [
      index.name,
      index.unique,
      index.multiEntry,
    ]),
    [
      ["byEmail", true, false],
      ["byTags", false, true],
    ],
  );
  const encodedValue = snapshot.databases["site-data"].stores.records.rows[0].value;
  assert.equal(encodedValue.t, "object");
  assert.equal(encodedValue.v.find(([key]) => key === "image")[1].t, "blob");
  assert.equal(encodedValue.v.find(([key]) => key === "attachment")[1].t, "file");

  const target = new FakeIndexedDB();
  assert.equal(await importSnapshot(target, snapshot), true);
  const importedRecord = target.records.get("site-data").stores.get("records");
  const importedLogs = target.records.get("site-data").stores.get("logs");
  assert.equal(importedRecord.keyPath, "id");
  assert.equal(importedRecord.autoIncrement, true);
  assert.deepEqual([...importedRecord.indexes.values()], [
    { name: "byEmail", keyPath: "email", unique: true, multiEntry: false },
    { name: "byTags", keyPath: "tags", unique: false, multiEntry: true },
  ]);
  assert.equal(importedLogs.keyPath, null);
  assert.equal(importedLogs.autoIncrement, true);
  assert.deepEqual([...importedRecord.records.values()][0].key, 1);
  const restored = [...importedRecord.records.values()][0].value;
  assert.ok(restored.image instanceof Blob);
  assert.equal(restored.image.type, "image/png");
  assert.deepEqual([...new Uint8Array(await restored.image.arrayBuffer())], [0, 1, 2, 255]);
  assert.ok(restored.attachment instanceof File);
  assert.equal(restored.attachment.name, "notes.txt");
  assert.equal(restored.attachment.lastModified, 123456);
  assert.equal(await restored.attachment.text(), "episode notes");
  assert.equal(restored.createdAt.toISOString(), "2025-03-04T05:06:07.000Z");
  assert.deepEqual([...restored.bytes], [1, 512, 65535]);
  assert.deepEqual([...restored.map.entries()], [["status", "watched"]]);
  assert.deepEqual([...restored.set.values()], ["dub", "sub"]);
  assert.equal(restored.expression.source, "episode");
  assert.equal(restored.expression.flags, "gi");
  assert.equal(Object.hasOwn(restored.dangerousObject, "__proto__"), true);
  assert.equal(restored.dangerousObject.__proto__.polluted, true);
  assert.equal(Object.prototype.polluted, undefined);
  assert.equal([...importedLogs.records.values()][0].value.event, "opened");
});

test("v2 import replaces an existing mismatched schema before restoring rows", async () => {
  const target = new FakeIndexedDB();
  const staleDb = target.seed("reconcile", 2);
  staleDb.createObjectStore("old-store", { keyPath: "oldId" });
  const snapshot = {
    format: "animehub-idb",
    version: 2,
    databases: {
      reconcile: {
        version: 2,
        stores: {
          records: {
            keyPath: "id",
            autoIncrement: false,
            indexes: [{ name: "byId", keyPath: "id", unique: true, multiEntry: false }],
            rows: [{ key: 4, value: { t: "object", nullPrototype: false, v: [["id", 4]] } }],
          },
        },
      },
    },
  };

  assert.equal(await importSnapshot(target, snapshot), true);
  const restoredDb = target.records.get("reconcile");
  assert.equal(restoredDb.version, 3);
  assert.equal(restoredDb.stores.has("old-store"), false);
  const restoredStore = restoredDb.stores.get("records");
  assert.equal(restoredStore.keyPath, "id");
  assert.equal(restoredStore.index("byId").unique, true);
  assert.equal([...restoredStore.records.values()][0].value.id, 4);
});

test("export rejects cyclic structured-clone values instead of silently losing them", async () => {
  const indexedDB = new FakeIndexedDB();
  const database = indexedDB.seed("cyclic", 1);
  const store = database.createObjectStore("records", { keyPath: "id" });
  const value = { id: 1 };
  value.self = value;
  store.putDirect(1, value);

  const exportFunction = vm.runInNewContext(`(${EXPORT_JS})`, jsContext(indexedDB));
  await assert.rejects(exportFunction(), /cyclic value/);
});

test("v1 snapshots remain importable into an existing inline-key store", async () => {
  const target = new FakeIndexedDB();
  const targetDb = target.seed("legacy-site", 2);
  const records = targetDb.createObjectStore("records", { keyPath: "id" });
  records.createIndex("byName", "name", { unique: true });
  records.putDirect(7, { id: 7, name: "old row" });

  const legacy = {
    "legacy-site": {
      version: 1,
      stores: {
        records: [
          { key: 7, value: { id: 7, name: "restored legacy row" } },
        ],
      },
    },
  };
  assert.equal(await importSnapshot(target, legacy), true);
  const imported = target.records.get("legacy-site").stores.get("records");
  assert.equal(imported.keyPath, "id");
  assert.equal(imported.index("byName").unique, true);
  assert.deepEqual([...imported.records.values()], [
    { key: 7, value: { id: 7, name: "restored legacy row" } },
  ]);
});
