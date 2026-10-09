/**
 * A very small fake DOM for `tests/pip_controller.test.js`.
 *
 * It exists to exercise the real, shipped `src-tauri/src/web/pip_controller.js`
 * logic — candidate scoring, the applied view and the restore — in Node. It is
 * deliberately not a browser: only the surface the controller touches
 * (`getBoundingClientRect`, `getComputedStyle`, inline `style`, `classList`,
 * `checkVisibility`, `querySelectorAll('video'|'iframe')`, `fullscreenElement`)
 * is implemented, so a new dependency in the controller shows up as a failing
 * test instead of an untested path.
 */

class FakeStyle {
  constructor() {
    this.props = new Map();
  }

  setProperty(name, value, priority) {
    this.props.set(String(name), {
      value: String(value),
      priority: priority ? String(priority) : "",
    });
  }

  getPropertyValue(name) {
    const entry = this.props.get(String(name));
    return entry ? entry.value : "";
  }

  getPropertyPriority(name) {
    const entry = this.props.get(String(name));
    return entry ? entry.priority : "";
  }

  removeProperty(name) {
    this.props.delete(String(name));
  }

  /** `style` attribute serialisation, the way a browser writes it back. */
  serialize() {
    const parts = [];
    for (const [name, entry] of this.props) {
      parts.push(entry.priority ? `${name}: ${entry.value} !important` : `${name}: ${entry.value}`);
    }
    return parts.join("; ");
  }

  parse(text) {
    this.props.clear();
    for (const chunk of String(text).split(";")) {
      const index = chunk.indexOf(":");
      if (index < 0) continue;
      const name = chunk.slice(0, index).trim();
      let value = chunk.slice(index + 1).trim();
      let priority = "";
      const bang = value.toLowerCase().indexOf("!important");
      if (bang >= 0) {
        priority = "important";
        value = value.slice(0, bang).trim();
      }
      if (name) this.props.set(name, { value, priority });
    }
  }
}

/** Minimal event targets: add/remove/dispatch, enough for the probe tests. */
function installListeners(obj) {
  const map = new Map();
  obj.addEventListener = (name, handler) => {
    if (!map.has(name)) map.set(name, new Set());
    map.get(name).add(handler);
  };
  obj.removeEventListener = (name, handler) => {
    if (map.has(name)) map.get(name).delete(handler);
  };
  obj.dispatch = (name) => {
    for (const handler of [...(map.has(name) ? map.get(name) : [])]) handler({ type: name });
  };
  obj.listenerCount = (name) => (map.has(name) ? map.get(name).size : 0);
  return obj;
}

class FakeElement {
  constructor(tagName, doc) {
    installListeners(this);
    this.tagName = String(tagName).toUpperCase();
    this.ownerDocument = doc;
    this.parentNode = null;
    this.children = [];
    this.attributes = new Map();
    this.classes = new Set();
    /** Overrides merged over the default computed style. */
    this.computed = {};
    this.rect = { left: 0, top: 0, width: 0, height: 0 };
    this.visibleOverride = null;
    this.style = new FakeStyle();
    const self = this;
    this.classList = {
      add(name) {
        self.classes.add(String(name));
      },
      remove(name) {
        self.classes.delete(String(name));
      },
      contains(name) {
        return self.classes.has(String(name));
      },
      toString() {
        return Array.from(self.classes).join(" ");
      },
    };
  }

  get isConnected() {
    let node = this;
    while (node) {
      if (node === this.ownerDocument.documentElement) return true;
      node = node.parentNode;
    }
    return false;
  }

  get parentElement() {
    return this.parentNode;
  }

  appendChild(child) {
    child.parentNode = this;
    this.children.push(child);
    return child;
  }

  removeChild(child) {
    const index = this.children.indexOf(child);
    if (index >= 0) this.children.splice(index, 1);
    child.parentNode = null;
    return child;
  }

  setAttribute(name, value) {
    const key = String(name);
    if (key === "style") {
      this.style.parse(value);
      this.attributes.set(key, this.style.serialize());
      return;
    }
    this.attributes.set(key, String(value));
  }

  getAttribute(name) {
    const key = String(name);
    if (key === "style") {
      const text = this.style.serialize();
      return text === "" && !this.attributes.has(key) ? null : text;
    }
    return this.attributes.has(key) ? this.attributes.get(key) : null;
  }

  removeAttribute(name) {
    const key = String(name);
    if (key === "style") this.style.props.clear();
    this.attributes.delete(key);
  }

  hasAttribute(name) {
    return this.attributes.has(String(name));
  }

  contains(node) {
    if (node === this) return true;
    for (const child of this.children) {
      if (child.contains(node)) return true;
    }
    return false;
  }

  getBoundingClientRect() {
    const r = this.rect;
    return {
      left: r.left,
      top: r.top,
      width: r.width,
      height: r.height,
      right: r.left + r.width,
      bottom: r.top + r.height,
    };
  }

  /** Mirrors the modern `Element.checkVisibility()` contract. */
  checkVisibility(options = {}) {
    if (this.visibleOverride !== null) return Boolean(this.visibleOverride);
    const style = this.ownerDocument.getComputedStyle(this);
    if (style.display === "none") return false;
    if (options.checkVisibilityCSS && style.visibility !== "visible") return false;
    if (options.checkOpacity && Number(style.opacity) <= 0.05) return false;
    return true;
  }

  querySelectorAll(selector) {
    const wanted = String(selector).split(",").map((s) => s.trim().toUpperCase());
    const out = [];
    const visit = (node) => {
      for (const child of node.children) {
        if (wanted.includes(child.tagName)) out.push(child);
        visit(child);
      }
    };
    visit(this);
    return out;
  }
}

class FakeDocument {
  constructor() {
    this.documentElement = new FakeElement("html", this);
    this.head = new FakeElement("head", this);
    this.body = new FakeElement("body", this);
    this.documentElement.appendChild(this.head);
    this.documentElement.appendChild(this.body);
    this.fullscreenElement = null;
  }

  createElement(tagName) {
    return new FakeElement(tagName, this);
  }

  getElementById(id) {
    const wanted = String(id);
    let found = null;
    const visit = (node) => {
      for (const child of node.children) {
        if (child.getAttribute("id") === wanted) found = child;
        visit(child);
      }
    };
    visit(this.documentElement);
    return found;
  }

  querySelectorAll(selector) {
    return this.documentElement.querySelectorAll(selector);
  }

  contains(node) {
    return this.documentElement.contains(node);
  }

  /** Computed style with the two inherited properties the controller reads. */
  getComputedStyle(el) {
    const base = { display: "block", visibility: "visible", opacity: "1" };
    const own = Object.assign(base, el.computed);
    let node = el.parentNode;
    while (node) {
      const inherited = node.computed && node.computed.visibility;
      if (inherited) own.visibility = inherited;
      node = node.parentNode;
    }
    return own;
  }
}

/**
 * Build a fake window/document pair.
 *
 * @param {{width?: number, height?: number, devicePixelRatio?: number}} options
 */
export function createFakeDom(options = {}) {
  const doc = installListeners(new FakeDocument());
  const win = installListeners({
    innerWidth: options.width === undefined ? 1080 : options.width,
    innerHeight: options.height === undefined ? 2400 : options.height,
    devicePixelRatio: options.devicePixelRatio === undefined ? 3 : options.devicePixelRatio,
    getComputedStyle: (el) => doc.getComputedStyle(el),
  });
  return { window: win, document: doc };
}

/** Create an element and attach it to `parent` (documentElement by default). */
export function add(doc, parent, element) {
  (parent || doc.body).appendChild(element);
  return element;
}

/** Place a box at `x,y` with a `width x height` rect. */
export function place(element, x, y, width, height) {
  element.rect = { left: x, top: y, width, height };
  return element;
}

/** A `<video>` element with playback state, appended to `parent`. */
export function addVideo(doc, parent, options = {}) {
  const video = doc.createElement("video");
  const rect = options.rect || { x: 0, y: 0, width: 360, height: 203 };
  place(video, rect.x, rect.y, rect.width, rect.height);
  video.readyState = options.readyState === undefined ? 4 : options.readyState;
  video.paused = options.paused === undefined ? false : options.paused;
  video.ended = Boolean(options.ended);
  video.currentTime = options.currentTime === undefined ? 30 : options.currentTime;
  video.videoWidth = options.videoWidth === undefined ? 1920 : options.videoWidth;
  video.videoHeight = options.videoHeight === undefined ? 1080 : options.videoHeight;
  if (options.computed) video.computed = options.computed;
  if (options.visibleOverride !== undefined) video.visibleOverride = options.visibleOverride;
  if (options.id) video.setAttribute("id", options.id);
  return add(doc, parent, video);
}

/** An `<iframe>` element, appended to `parent`. */
export function addIframe(doc, parent, options = {}) {
  const frame = doc.createElement("iframe");
  const rect = options.rect || { x: 0, y: 0, width: 360, height: 203 };
  place(frame, rect.x, rect.y, rect.width, rect.height);
  if (options.src) frame.setAttribute("src", options.src);
  if (options.allow) frame.setAttribute("allow", options.allow);
  if (options.computed) frame.computed = options.computed;
  return add(doc, parent, frame);
}

/** A plain `<div>` (page chrome, ad slots, player wrappers). */
export function addDiv(doc, parent, options = {}) {
  const div = doc.createElement("div");
  const rect = options.rect || { x: 0, y: 0, width: 0, height: 0 };
  place(div, rect.x, rect.y, rect.width, rect.height);
  if (options.computed) div.computed = options.computed;
  if (options.visibleOverride !== undefined) div.visibleOverride = options.visibleOverride;
  if (options.id) div.setAttribute("id", options.id);
  if (options.style) div.setAttribute("style", options.style);
  return add(doc, parent, div);
}
